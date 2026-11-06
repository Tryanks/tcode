//! Native OpenCode provider for OpenCode 2.x: one private `opencode serve
//! --stdio` per Tcode session, the server's `/api` routes, and its
//! `/api/event` SSE stream.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Map, Value, json};
use smol::channel::{Receiver, Sender};
use smol::future;

use crate::actor::{self, EventSenderExt as _, SessionActor, TransportOutcome};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, Attachment, ChangeCompleteness, Compaction, DeltaKind, FileChange,
    FileChangeKind, ItemContent, ItemStatus, LaunchEnv, ModelSpec, OptionDescriptor,
    OptionSelection, PlanStep, PlanStepStatus, ProviderCommand, ProviderCommandKind, ProviderKind,
    ResumeCursor, RewindMode, SelectOption, SessionCommand, SessionHandle, SessionOptions,
    ThreadItem, TokenUsage, ToolOutput, UserInputDelivery, UserInputOption, UserInputQuestion,
};

/// The directory OpenCode launched with `launch_env` keeps its sessions,
/// database and sign-ins in: `opencode` under `XDG_DATA_HOME`, else under
/// `~/.local/share`, on every platform.
pub fn data_dir(launch_env: &LaunchEnv) -> Option<PathBuf> {
    launch_env
        .child_path(ProviderKind::OpenCode, "XDG_DATA_HOME")
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")))
        .map(|data| data.join("opencode"))
}

/// The first release with the server API this adapter speaks; 1.x servers
/// expose an incompatible one.
const MIN_VERSION: (u32, u32, u32) = (2, 0, 0);
const PERMISSION_MODE: &str = "permission_mode";
const EFFORT: &str = "reasoningEffort";
const AGENT: &str = "agent";
/// Tcode's entry among the session's API-managed instructions.
const INSTRUCTIONS_KEY: &str = "tcode";
/// A first start migrates OpenCode's database before the server listens.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// The server streams no shell output; its own clients poll the shell's
/// output route while the command runs.
const SHELL_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    require_supported_version(opts.binary_path.as_deref(), &opts.launch_env).await?;
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
    require_supported_version(binary_path.as_deref(), &launch_env).await?;
    crate::process::unblock(move || {
        let cwd = std::env::current_dir()?;
        let mut server = Server::spawn(binary_path.as_deref(), &cwd, &launch_env, &[], &[])?;
        let result = (|| {
            let models = server.http.get("/api/model")?;
            let default = server.http.get("/api/model/default").ok();
            Ok(map_models(
                &models["data"],
                default.as_ref().map(|default| &default["data"]),
            ))
        })();
        server.stop();
        result
    })
    .await
}

async fn require_supported_version(
    binary_path: Option<&Path>,
    launch_env: &LaunchEnv,
) -> Result<(), AgentError> {
    let binary = crate::resolve_binary(binary_path, "opencode")?;
    match crate::process::probe_version(&binary, launch_env, ProviderKind::OpenCode).await {
        Some(version @ (major, minor, patch)) if version < MIN_VERSION => {
            Err(AgentError::Provider(format!(
                "OpenCode 2.0 or later is required, but `{}` is version {major}.{minor}.{patch}. \
                 Update OpenCode to use it with Tcode.",
                binary.display()
            )))
        }
        // An unreadable version leaves the failure to the server start itself.
        _ => Ok(()),
    }
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
    let mut mapper = Mapper::new(
        setup.session_id.clone(),
        setup.mcp_servers,
        setup.context_windows,
    );
    mapper.served_model = model_id.clone();
    let mut actor = OpenCodeActor {
        server,
        http: setup.http,
        transport,
        transport_tx,
        events,
        mapper,
        model: setup.model,
        variant: crate::selection_str(&opts.option_selections, EFFORT),
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
        actor.shutdown().await;
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
    let session = match resumed {
        Some(session_id) if opts.fork => {
            http.post(&format!("/api/session/{session_id}/fork"), &json!({}))?
        }
        Some(session_id) => http.get(&format!("/api/session/{session_id}"))?,
        None => {
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
        Some(directory) => http.at(Path::new(directory)),
        None => http.clone(),
    };
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
        model = http
            .get("/api/model/default")
            .ok()
            .and_then(|default| ModelRef::from_wire(&default["data"]));
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
        commands: discover_commands(&http),
        agents: http
            .get("/api/agent")
            .map(|agents| map_agents(&agents["data"]))
            .unwrap_or_default(),
        context_windows: http
            .get("/api/model")
            .map(|models| context_windows(&models["data"]))
            .unwrap_or_default(),
        mcp_servers: mcp_servers(&http),
        session_id,
        http,
        model,
        agent,
    })
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

#[derive(Debug, Clone)]
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
fn mcp_servers(http: &Http) -> Vec<String> {
    http.get("/api/mcp")
        .ok()
        .and_then(|servers| servers["data"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|server| server["name"].as_str().map(str::to_owned))
        .collect()
}

fn discover_commands(http: &Http) -> Vec<ProviderCommand> {
    let listed = |path: &str| {
        http.get(path)
            .ok()
            .and_then(|value| value["data"].as_array().cloned())
            .unwrap_or_default()
    };
    let mut commands: Vec<ProviderCommand> = listed("/api/command")
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
    commands.extend(listed("/api/skill").iter().filter_map(|skill| {
        let id = skill["id"].as_str()?.trim();
        (!id.is_empty()).then(|| ProviderCommand {
            name: id.to_owned(),
            description: skill["description"].as_str().map(str::to_owned),
            kind: ProviderCommandKind::Skill,
        })
    }));
    commands
}

fn map_models(models: &Value, default: Option<&Value>) -> Vec<ModelSpec> {
    let default = default.and_then(ModelRef::from_wire);
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
                        })
                        .collect(),
                    default_value: None,
                    role: crate::OptionRole::Model,
                    apply: crate::ApplyTiming::Live,
                    recommended: None,
                    permissive: None,
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
    server: Server,
    http: Http,
    transport: Receiver<Transport>,
    /// Shell output pollers feed the same queue as the event stream.
    transport_tx: Sender<Transport>,
    events: Sender<AgentEvent>,
    mapper: Mapper,
    model: Option<ModelRef>,
    variant: Option<String>,
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
    shell_polls: HashMap<String, Arc<AtomicBool>>,
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
                let event = match self.rewind(&checkpoint_id, mode) {
                    Ok(prefill) => AgentEvent::RewindCompleted {
                        checkpoint_id,
                        mode,
                        prefill,
                    },
                    Err(error) => AgentEvent::RewindFailed {
                        checkpoint_id,
                        mode,
                        error,
                    },
                };
                self.events.emit(event).await;
            }
            SessionCommand::RetryTurn { .. }
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
            Ok(Transport::Event(event)) => {
                self.handle_event(&event).await;
                TransportOutcome::Continue
            }
            Ok(Transport::ShellOutput { item_id, text }) => {
                let events = self.mapper.shell_output(&item_id, text);
                for event in events {
                    self.events.emit(event).await;
                }
                TransportOutcome::Continue
            }
            Ok(Transport::Error(err)) => TransportOutcome::Fatal(err),
            Ok(Transport::Eof) | Err(_) => {
                TransportOutcome::Closed("OpenCode event stream closed".into())
            }
        }
    }

    async fn teardown(mut self, reason: Option<String>) -> Option<String> {
        self.shutdown().await;
        reason.map(|reason| self.server.tail.append_to(reason, "\nserver output:\n"))
    }
}

impl OpenCodeActor {
    fn session_path(&self, route: &str) -> String {
        format!("/api/session/{}/{route}", self.mapper.root)
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
                    })
                    .collect(),
                default_value: None,
                role: crate::OptionRole::Model,
                apply: crate::ApplyTiming::Live,
                recommended: None,
                permissive: None,
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
            ApprovalDecision::Option(id) if matches!(id.as_str(), "once" | "always" | "reject") => {
                (id.as_str(), None)
            }
            // A rejection with a message is OpenCode's correction: the model
            // continues from the user's words instead of stopping.
            ApprovalDecision::Feedback(text) => ("reject", Some(text.as_str())),
            ApprovalDecision::Cancel => ("reject", None),
            ApprovalDecision::Option(_) => return Ok(()),
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
    /// files its steps changed when asked; it has no files-only restore.
    fn rewind(&mut self, checkpoint_id: &str, mode: RewindMode) -> Result<Option<String>, String> {
        if !mode.includes_conversation() {
            return Err(
                "OpenCode restores files only together with the conversation that changed them"
                    .into(),
            );
        }
        let prefill = self
            .http
            .get(&self.session_path(&format!("message/{checkpoint_id}")))
            .ok()
            .and_then(|message| message.pointer("/data/text")?.as_str().map(str::to_owned));
        self.http
            .post(
                &self.session_path("revert/stage"),
                &json!({ "messageID": checkpoint_id, "files": mode.includes_files() }),
            )
            .map_err(|err| err.to_string())?;
        self.http
            .post_empty(&self.session_path("revert/commit"), &json!({}))
            .map_err(|err| err.to_string())?;
        self.prompts.remove(checkpoint_id);
        Ok(prefill)
    }

    async fn handle_event(&mut self, event: &Value) {
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
            "command.updated" | "skill.updated" => {
                let commands = discover_commands(&self.http);
                if commands != self.commands {
                    self.commands = commands.clone();
                    self.events
                        .emit(AgentEvent::ProviderCommands { commands })
                        .await;
                }
            }
            "mcp.status.changed" => self.mapper.mcp_servers = mcp_servers(&self.http),
            "agent.updated" => {
                if let Ok(agents) = self.http.get("/api/agent") {
                    self.agents = map_agents(&agents["data"]);
                    self.emit_options().await;
                }
            }
            "session.step.started" if data["sessionID"] == self.mapper.root.as_str() => {
                // A session without an agent of its own runs the configured default.
                if let Some(agent) = data["agent"].as_str()
                    && self.agent.as_deref() != Some(agent)
                {
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
        let mapped = self.mapper.on_event(event);
        let turn_completed = mapped
            .events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }));
        for event in mapped.events {
            self.events.emit(event).await;
        }
        for action in mapped.actions {
            self.perform(action).await;
        }
        if turn_completed {
            let root = self.mapper.root.clone();
            self.cancel_forms(Some(&root)).await;
        }
    }

    async fn perform(&mut self, action: Action) {
        match action {
            Action::PollShell {
                item_id,
                shell_id,
                directory,
            } => {
                let stop = Arc::new(AtomicBool::new(false));
                self.shell_polls.insert(item_id.clone(), stop.clone());
                let http = match directory {
                    Some(directory) => self.http.at(Path::new(&directory)),
                    None => self.http.clone(),
                };
                let sender = self.transport_tx.clone();
                let _ = std::thread::Builder::new()
                    .name("opencode-shell-output".into())
                    .spawn(move || poll_shell_output(&http, &shell_id, &item_id, &stop, &sender));
            }
            Action::StopShell { item_id } => {
                if let Some(stop) = self.shell_polls.remove(&item_id) {
                    stop.store(true, Ordering::Relaxed);
                }
            }
            Action::FetchDiff => match self.http.get(&self.session_path("diff")) {
                Ok(diff) => {
                    self.events
                        .emit(AgentEvent::TurnChangesUpdated {
                            turn_id: self.mapper.turn_id(),
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
            },
        }
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
        let request = ApprovalRequest {
            id: request_id.to_owned(),
            turn_id: self.mapper.active_turn.clone(),
            parent_item_id,
            kind: self.mapper.approval_kind(data),
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

    /// Cancels the forms still open in `session` (every session when `None`).
    async fn cancel_forms(&mut self, session: Option<&str>) {
        let pending: Vec<String> = self
            .forms
            .iter()
            .filter(|(_, form)| session.is_none_or(|session| form.session_id == session))
            .map(|(id, _)| id.clone())
            .collect();
        for form_id in pending {
            let Some(form) = self.forms.remove(&form_id) else {
                continue;
            };
            if let Err(err) = self
                .http
                .delete(&format!("/api/session/{}/form/{form_id}", form.session_id))
            {
                self.events
                    .emit(AgentEvent::Warning {
                        message: format!("failed to cancel OpenCode question {form_id}: {err}"),
                    })
                    .await;
            }
            self.events
                .emit(AgentEvent::UserInputResolved {
                    request_id: form_id,
                    answers: Map::new(),
                })
                .await;
        }
    }

    async fn shutdown(&mut self) {
        for stop in self.shell_polls.values() {
            stop.store(true, Ordering::Relaxed);
        }
        self.cancel_forms(None).await;
        for (request_id, session_id) in std::mem::take(&mut self.permissions) {
            let _ = self.http.post_empty(
                &format!("/api/session/{session_id}/permission/{request_id}/reply"),
                &json!({ "decision": "reject", "message": "Tcode session closed" }),
            );
        }
        self.server.stop();
    }
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

fn poll_shell_output(
    http: &Http,
    shell_id: &str,
    item_id: &str,
    stop: &AtomicBool,
    sender: &Sender<Transport>,
) {
    let mut cursor = 0;
    while !stop.load(Ordering::Relaxed) {
        let Ok(page) = http.get(&format!("/api/shell/{shell_id}/output?cursor={cursor}")) else {
            // The server drops a shell once its command is settled.
            return;
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
        std::thread::sleep(SHELL_POLL_INTERVAL);
    }
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
    FetchDiff,
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
    /// The subagent moved to the background; it settles with its session.
    background: bool,
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
    turn_counter: u64,
    active_turn: Option<String>,
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
            turn_counter: 0,
            active_turn: None,
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

    fn turn_id(&self) -> String {
        self.active_turn
            .clone()
            .or_else(|| self.last_turn.clone())
            .unwrap_or_else(|| "opencode-turn".into())
    }

    fn on_event(&mut self, event: &Value) -> Mapped {
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
            return mapped;
        }
        let Some(session_id) = data["sessionID"].as_str() else {
            return mapped;
        };
        let Some(parent_item_id) = self.scope(session_id) else {
            return mapped;
        };
        let root = session_id == self.root;
        let message_id = data["assistantMessageID"].as_str().unwrap_or_default();
        match kind {
            "session.execution.started" if root => {
                self.turn_processed = 0;
                self.turn_cost = None;
                self.last_error = None;
                mapped.events.extend(crate::start_mapped_turn(
                    "opencode",
                    &mut self.turn_counter,
                    &mut self.active_turn,
                    &mut self.turn_usage,
                    &mut self.turn_failed,
                ));
            }
            "session.execution.succeeded"
            | "session.execution.failed"
            | "session.execution.interrupted" => {
                let status = match kind {
                    "session.execution.succeeded" => ItemStatus::Completed,
                    "session.execution.failed" => ItemStatus::Failed,
                    _ => ItemStatus::Interrupted,
                };
                if root {
                    if status == ItemStatus::Failed {
                        self.turn_failed = true;
                        let message = error_message(&data["error"]);
                        if self.last_error.as_deref() != Some(message.as_str()) {
                            mapped.events.push(AgentEvent::Error {
                                message,
                                fatal: false,
                            });
                        }
                    } else if status == ItemStatus::Interrupted {
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
                } else {
                    mapped
                        .events
                        .extend(self.settle_background_subagent(session_id, status));
                }
            }
            "session.step.started" => {
                let model = ModelRef::from_wire(&data["model"]).map(|model| model.id());
                if root {
                    if let Some(model) = &model {
                        self.context_window = self.context_windows.get(model).copied();
                        if self.served_model.as_deref() != Some(model.as_str()) {
                            self.served_model = Some(model.clone());
                            mapped.events.push(AgentEvent::ServedModel {
                                model: model.clone(),
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
            "session.retry.scheduled" if root => mapped.events.push(AgentEvent::Warning {
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
                    return mapped;
                };
                if tool.name == "shell"
                    && let Some(shell_id) = metadata["shellID"].as_str()
                {
                    mapped.actions.push(Action::PollShell {
                        item_id: item_id.clone(),
                        shell_id: shell_id.to_owned(),
                        directory: event
                            .pointer("/location/directory")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
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
                mapped.actions.push(Action::StopShell {
                    item_id: item_id.clone(),
                });
                let Some(tool) = self.tools.get_mut(&item_id) else {
                    return mapped;
                };
                let success = kind == "session.tool.success";
                if success
                    && tool.name == "subagent"
                    && data.pointer("/metadata/status") == Some(&json!("running"))
                {
                    tool.background = true;
                    let item = self.tool_item(&item_id, ItemStatus::InProgress, Some(data), None);
                    if let Some(item) = item {
                        let backgrounded = AgentEvent::ItemBackgrounded {
                            item_id: item.id.clone(),
                            parent_item_id: item.parent_item_id.clone(),
                        };
                        mapped.events.push(AgentEvent::ItemUpdated(item));
                        mapped.events.push(backgrounded);
                    }
                    return mapped;
                }
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
                    return mapped;
                }
                let status = if success {
                    ItemStatus::Completed
                } else if data.pointer("/error/type") == Some(&json!("aborted")) {
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
        mapped
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

    fn settle_background_subagent(&mut self, child: &str, status: ItemStatus) -> Vec<AgentEvent> {
        let Some(item_id) = self.children.get(child).cloned().flatten() else {
            return Vec::new();
        };
        if !self.tools.get(&item_id).is_some_and(|tool| tool.background) {
            return Vec::new();
        }
        let item = self.tool_item(&item_id, status, None, None);
        self.tools.remove(&item_id);
        item.map(AgentEvent::ItemCompleted).into_iter().collect()
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
                output: text
                    .or_else(|| (!tool.live_output.is_empty()).then(|| tool.live_output.clone()))
                    .or(error.filter(|_| status == ItemStatus::Failed))
                    .unwrap_or_default(),
                exit_code: metadata
                    .and_then(|metadata| metadata["exit"].as_i64())
                    .and_then(|exit| i32::try_from(exit).ok()),
                status,
            },
            "edit" | "write" | "patch" => ItemContent::FileChange {
                changes: file_changes(&tool.name, input, text.as_deref(), metadata),
                status,
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
            name => ItemContent::ToolCall {
                image_reads: images,
                name: name.to_owned(),
                input: input.clone(),
                output: if status == ItemStatus::Completed {
                    text
                } else {
                    error.or(text)
                },
                status,
            },
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
                reason,
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
                reason,
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
    child: Child,
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
    ) -> Result<Self, AgentError> {
        let binary = crate::resolve_binary(binary_path, "opencode")?;
        let password = uuid::Uuid::new_v4().simple().to_string();
        let mut command = crate::process::command(&binary);
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
        let mut child = command
            .spawn()
            .map_err(|err| AgentError::Spawn(format!("spawning `{}`: {err}", binary.display())))?;
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
        let announced = match receiver.recv_timeout(STARTUP_TIMEOUT) {
            Ok((Ok(line), stdout)) => {
                let _ = server.tail.spawn(stdout, "opencode-stdout", "OpenCode");
                line
            }
            Ok((Err(err), _)) => {
                return Err(server.startup_error(format!("reading OpenCode startup: {err}")));
            }
            Err(_) => {
                return Err(server.startup_error(
                    "timed out waiting for the OpenCode server to announce its address".into(),
                ));
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
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let mut last = String::from("no response");
        while Instant::now() < deadline {
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
                Ok(status) => last = format!("HTTP {status}"),
                Err(err) => last = err.to_string(),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(self.startup_error(format!("timed out waiting for OpenCode to start: {last}")))
    }

    fn startup_error(&mut self, message: String) -> AgentError {
        // The output readers reach EOF only once the child is gone.
        self.stop();
        AgentError::Provider(self.tail.append_to(message, "\nserver output:\n"))
    }

    fn subscribe(&self) -> Result<(Receiver<Transport>, Sender<Transport>), AgentError> {
        // The event stream idles between turns (with a heartbeat every 15 s),
        // so it gets its own agent without the REST read timeout.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(1))
            .timeout_read(Duration::from_secs(24 * 60 * 60))
            .build();
        let response = agent
            .get(&format!("{}/api/event", self.http.base))
            .set("Authorization", &self.http.authorization)
            .call()
            .map_err(http_error)?;
        let (sender, receiver) = smol::channel::unbounded();
        let reader_sender = sender.clone();
        std::thread::Builder::new()
            .name("opencode-sse".into())
            .spawn(move || read_sse(response.into_reader(), reader_sender))
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
    directory: String,
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
            directory: uri_component(&directory.to_string_lossy()),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(1))
                .timeout_read(Duration::from_secs(60))
                .timeout_write(Duration::from_secs(10))
                .build(),
        }
    }

    fn at(&self, directory: &Path) -> Self {
        Self {
            directory: uri_component(&directory.to_string_lossy()),
            ..self.clone()
        }
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        self.agent
            .request(method, &format!("{}{path}", self.base))
            .set("Authorization", &self.authorization)
            .set("x-opencode-directory", &self.directory)
    }

    fn get(&self, path: &str) -> Result<Value, AgentError> {
        parse_response(self.request("GET", path).call().map_err(http_error)?)
    }

    fn status(&self, path: &str) -> Result<u16, AgentError> {
        match self.request("GET", path).call() {
            Ok(response) => Ok(response.status()),
            Err(ureq::Error::Status(status, _)) => Ok(status),
            Err(err) => Err(http_error(err)),
        }
    }

    fn send(&self, method: &str, path: &str, body: &Value) -> Result<Value, AgentError> {
        parse_response(
            self.request(method, path)
                .set("Content-Type", "application/json")
                .send_string(&body.to_string())
                .map_err(http_error)?,
        )
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
        parse_response(self.request("DELETE", path).call().map_err(http_error)?).map(drop)
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
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|body| body["message"].as_str().map(str::to_owned))
                .unwrap_or(body);
            AgentError::Provider(format!(
                "OpenCode HTTP {status}: {}",
                if detail.trim().is_empty() {
                    "empty response"
                } else {
                    detail.trim()
                }
            ))
        }
        ureq::Error::Transport(error) => AgentError::Protocol(format!("OpenCode HTTP: {error}")),
    }
}

enum Transport {
    Event(Value),
    ShellOutput { item_id: String, text: String },
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
    future::race(
        async {
            loop {
                match events.recv().await {
                    Ok(Transport::Event(event)) if event["type"] == "server.connected" => {
                        return Ok(());
                    }
                    Ok(Transport::Event(_) | Transport::ShellOutput { .. }) => {}
                    Ok(Transport::Error(error)) => return Err(AgentError::Protocol(error)),
                    Ok(Transport::Eof) | Err(_) => {
                        return Err(AgentError::Protocol(
                            "OpenCode event stream closed before server.connected".into(),
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

#[cfg(all(test, unix))]
mod tests {
    use std::collections::VecDeque;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Condvar, Mutex};

    use super::*;
    use crate::{McpRegistration, TurnOptions};

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/opencode");

    fn fixture_text(name: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap()
    }

    /// A recorded `/api/event` frame sequence, one event per line.
    fn frames(name: &str) -> Vec<Value> {
        fixture_text(name)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// One step of the fake server's event stream.
    enum Step {
        Frame(Value),
        /// Holds the stream until the client has made `count` requests of
        /// this kind (`METHOD /path?query`), as the real server answers them.
        After(String, usize),
    }

    /// The recorded frames, the stream held before a frame of each listed type
    /// until the request that caused it on the real server has been made.
    fn script(name: &str, waits: &[(&str, &str, usize)]) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for frame in frames(name) {
            let kind = frame["type"].as_str().unwrap().to_owned();
            let occurrence = seen.entry(kind.clone()).or_default();
            *occurrence += 1;
            for (before, request, count) in waits {
                // The `count`-th request precedes the `count`-th such frame.
                if kind == *before && *occurrence == *count {
                    steps.push(Step::After((*request).into(), *count));
                }
            }
            steps.push(Step::Frame(frame));
        }
        steps
    }

    const PROMPT: &str = "POST /api/session/ses_root/prompt";

    #[derive(Debug, Clone)]
    struct Request {
        key: String,
        body: Value,
    }

    #[derive(Default)]
    struct State {
        requests: Vec<Request>,
        routes: HashMap<String, VecDeque<(u16, String)>>,
    }

    /// Stands in for `opencode serve`: answers routes from recorded
    /// responses, plays a recorded event stream and records every request.
    struct FakeServer {
        url: String,
        state: Arc<(Mutex<State>, std::sync::Condvar)>,
    }

    impl FakeServer {
        fn start(steps: Vec<Step>, routes: Vec<(&str, u16, String)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let mut state = State::default();
            for (key, status, body) in routes {
                state
                    .routes
                    .entry(key.into())
                    .or_default()
                    .push_back((status, body));
            }
            let state = Arc::new((Mutex::new(state), Condvar::new()));
            let shared = state.clone();
            let steps = Arc::new(Mutex::new(Some(steps)));
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let shared = shared.clone();
                    let steps = steps.clone();
                    std::thread::spawn(move || serve(stream, &shared, &steps));
                }
            });
            Self { url, state }
        }

        fn requests(&self) -> Vec<Request> {
            self.state.0.lock().unwrap().requests.clone()
        }

        /// The requests that are not reads, in order.
        fn writes(&self) -> Vec<(String, Value)> {
            self.requests()
                .into_iter()
                .filter(|request| !request.key.starts_with("GET "))
                .map(|request| (request.key, request.body))
                .collect()
        }
    }

    fn serve(
        mut stream: TcpStream,
        state: &(Mutex<State>, Condvar),
        steps: &Mutex<Option<Vec<Step>>>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let mut length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        if path == "/api/event" {
            let connected = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"id\":\"evt_connected\",\"type\":\"server.connected\",\"data\":{}}\n\n";
            stream.write_all(connected).unwrap();
            for step in steps.lock().unwrap().take().unwrap_or_default() {
                match step {
                    Step::Frame(frame) => {
                        if stream
                            .write_all(format!("data: {frame}\n\n").as_bytes())
                            .is_err()
                        {
                            return;
                        }
                    }
                    Step::After(key, count) => {
                        let guard = state.0.lock().unwrap();
                        let (_guard, wait) = state
                            .1
                            .wait_timeout_while(guard, Duration::from_secs(10), |state| {
                                state.requests.iter().filter(|r| r.key == key).count() < count
                            })
                            .unwrap();
                        if wait.timed_out() {
                            // The client never asked; the test sees the stream stall.
                            return;
                        }
                    }
                }
            }
            // Hold the stream open until the client goes away.
            let _ = reader.read(&mut [0; 1]);
            return;
        }
        let key = format!("{method} {path}");
        let (status, response) = {
            let mut guard = state.0.lock().unwrap();
            if path != "/api/info" {
                guard.requests.push(Request {
                    key: key.clone(),
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                });
                state.1.notify_all();
            }
            match guard.routes.get_mut(&key) {
                Some(queue) if queue.len() > 1 => queue.pop_front().unwrap(),
                Some(queue) => queue.front().cloned().unwrap(),
                None if method == "GET" => (200, r#"{"data":[]}"#.into()),
                None => (204, String::new()),
            }
        };
        let reason = match status {
            204 => "No Content",
            404 => "Not Found",
            _ => "OK",
        };
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .as_bytes(),
        );
    }

    /// A stand-in `opencode` that reports `TCODE_TEST_OPENCODE_VERSION`, keeps
    /// the inline config it was started with in `config`, and announces the
    /// fake server the way `serve --stdio` does.
    fn fake_opencode(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(directory).unwrap();
        let binary = directory.join("opencode");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
if [ "$1" = --version ]; then printf '%s\n' "$TCODE_TEST_OPENCODE_VERSION"; exit 0; fi
printf '%s' "$OPENCODE_CONFIG_CONTENT" > "$TCODE_TEST_OPENCODE_DIR/config"
if [ -n "$TCODE_TEST_OPENCODE_FAIL" ]; then printf '%s\n' "$TCODE_TEST_OPENCODE_FAIL" >&2; exit 1; fi
printf '{"url":"%s"}\n' "$TCODE_TEST_OPENCODE_URL"
cat > /dev/null
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        binary
    }

    struct Fixture {
        directory: PathBuf,
        server: FakeServer,
        options: SessionOptions,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    /// The created session, as `POST /api/session` answered it.
    const SESSION: &str = r#"{"data":{"id":"ses_root","projectID":"5bbc9219e3603f1ec4d88b8dc971434e6571740d","agent":"build","model":{"id":"deepseek-v4.1-flash-free","providerID":"opencode","variant":"low"},"cost":0,"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1793562629343,"updated":1793562629343},"location":{"directory":"/work/project"}}}"#;

    fn fixture(steps: Vec<Step>, mut routes: Vec<(&str, u16, String)>) -> Fixture {
        let directory =
            std::env::temp_dir().join(format!("agent-opencode-{}", uuid::Uuid::new_v4()));
        let binary = fake_opencode(&directory);
        routes.push(("POST /api/session", 200, SESSION.into()));
        let server = FakeServer::start(steps, routes);
        let options = SessionOptions {
            cwd: directory.clone(),
            model: Some("opencode/deepseek-v4.1-flash-free".into()),
            resume: None,
            fork: false,
            binary_path: Some(binary),
            option_selections: vec![OptionSelection {
                id: EFFORT.into(),
                value: json!("low"),
            }],
            mcp_servers: Vec::new(),
            launch_env: LaunchEnv {
                env: vec![
                    ("TCODE_TEST_OPENCODE_VERSION".into(), "2.2.5".into()),
                    ("TCODE_TEST_OPENCODE_URL".into(), server.url.clone()),
                    (
                        "TCODE_TEST_OPENCODE_DIR".into(),
                        directory.to_string_lossy().into_owned(),
                    ),
                ],
                home: None,
            },
            extra_args: Vec::new(),
            acp: None,
            instructions: None,
        };
        Fixture {
            directory,
            server,
            options,
        }
    }

    /// A user prompt's admission, as the server answers `POST …/prompt`.
    fn inbox(id: &str) -> String {
        format!(
            r#"{{"data":{{"id":"{id}","sessionID":"ses_root","time":{{"created":1793562692083}},"type":"user","payload":{{"text":"t"}},"delivery":"queue"}}}}"#
        )
    }

    async fn recv(handle: &SessionHandle) -> AgentEvent {
        future::race(async { handle.events.recv().await.unwrap() }, async {
            smol::Timer::after(Duration::from_secs(10)).await;
            panic!("no event within 10 s")
        })
        .await
    }

    /// Starts a session and drains its three startup events.
    async fn open(options: SessionOptions) -> SessionHandle {
        let handle = start(options).await.unwrap();
        for _ in 0..3 {
            recv(&handle).await;
        }
        handle
    }

    /// Events up to and including the first one `last` accepts, as wire JSON.
    async fn until(handle: &SessionHandle, last: impl Fn(&AgentEvent) -> bool) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            let event = recv(handle).await;
            let done = last(&event);
            events.push(serde_json::to_value(&event).unwrap());
            if done {
                return events;
            }
        }
    }

    fn turn_completed(event: &AgentEvent) -> bool {
        matches!(event, AgentEvent::TurnCompleted { .. })
    }

    async fn send(handle: &SessionHandle, command: SessionCommand) {
        handle.commands.send(command).await.unwrap();
    }

    fn turn(text: &str) -> SessionCommand {
        SessionCommand::SendTurn {
            delivery_id: 1,
            text: text.into(),
            options: None,
            attachments: Vec::new(),
        }
    }

    async fn shut_down(handle: SessionHandle) {
        send(&handle, SessionCommand::Shutdown).await;
        while let Ok(event) = handle.events.recv().await {
            if matches!(event, AgentEvent::SessionClosed { .. }) {
                break;
            }
        }
    }

    /// The events a turn's bookkeeping adds around its items.
    fn items_only(events: Vec<Value>) -> Vec<Value> {
        events
            .into_iter()
            .filter(|event| {
                !matches!(
                    event["type"].as_str(),
                    Some("token_usage" | "turn_accepted" | "turn_started" | "turn_checkpoint")
                )
            })
            .collect()
    }

    #[test]
    fn a_1x_opencode_is_refused_at_start_naming_the_2_0_requirement() {
        smol::block_on(async {
            let fixture = fixture(Vec::new(), Vec::new());
            let mut options = fixture.options.clone();
            options.launch_env.env[0].1 = "1.19.5".into();
            let Err(error) = start(options.clone()).await else {
                panic!("a 1.x server must not start");
            };
            assert_eq!(
                error.to_string(),
                format!(
                    "provider reported error: OpenCode 2.0 or later is required, but `{}` is \
                     version 1.19.5. Update OpenCode to use it with Tcode.",
                    options.binary_path.unwrap().display()
                )
            );
            assert!(!fixture.directory.join("config").exists(), "no server ran");
        });
    }

    #[test]
    fn a_server_that_exits_during_startup_reports_its_own_output() {
        smol::block_on(async {
            let fixture = fixture(Vec::new(), Vec::new());
            let mut options = fixture.options.clone();
            options.launch_env.env.push((
                "TCODE_TEST_OPENCODE_FAIL".into(),
                "database is locked".into(),
            ));
            let Err(error) = start(options).await else {
                panic!("the server exited");
            };
            assert_eq!(
                error.to_string(),
                "provider reported error: OpenCode server exited during startup\n\
                 server output:\ndatabase is locked"
            );
        });
    }

    #[test]
    fn a_text_turn_streams_the_reply_and_reports_its_usage() {
        smol::block_on(async {
            let fixture = fixture(
                script("text_turn.jsonl", &[("session.inbox.enqueued", PROMPT, 1)]),
                vec![(PROMPT, 200, inbox("msg_198aac9f2001K4cb8mDh24jTzH"))],
            );
            let mut options = fixture.options.clone();
            options.instructions =
                Some("<pull_request_linking>\nLink every PR.\n</pull_request_linking>\n\n".into());
            let handle = start(options).await.unwrap();
            let mut startup = Vec::new();
            for _ in 0..3 {
                startup.push(serde_json::to_value(recv(&handle).await).unwrap());
            }
            assert_eq!(
                startup,
                vec![
                    json!({"type": "session_started", "provider_session_id": "ses_root", "resume": {"session_id": "ses_root"}, "model": "opencode/deepseek-v4.1-flash-free"}),
                    json!({"type": "provider_options", "descriptors": [crate::permission_control(ProviderKind::OpenCode)], "selections": [{"id": "permission_mode", "value": "normal"}]}),
                    json!({"type": "provider_commands", "commands": []}),
                ]
            );
            send(&handle, turn("Reply with exactly: PONG")).await;
            assert_eq!(
                until(&handle, turn_completed).await,
                vec![
                    json!({"type": "turn_accepted", "delivery_id": 1}),
                    json!({"type": "turn_started", "turn_id": "opencode-turn-1"}),
                    json!({"type": "turn_checkpoint", "turn_id": "opencode-turn-1", "checkpoint_id": "msg_198aac9f2001K4cb8mDh24jTzH"}),
                    json!({"type": "item_started", "id": "msg_198aaca0a001IZCSKwcEekLiz8:text:0", "content": {"kind": "assistant_message", "text": ""}}),
                    json!({"type": "delta", "item_id": "msg_198aaca0a001IZCSKwcEekLiz8:text:0", "kind": "assistant_text", "text": "PONG"}),
                    json!({"type": "item_completed", "id": "msg_198aaca0a001IZCSKwcEekLiz8:text:0", "content": {"kind": "assistant_message", "text": "PONG"}}),
                    json!({"type": "token_usage", "freshness": "current", "turn_processed_tokens": null, "input_tokens": 6335, "cached_input_tokens": 3328, "output_tokens": 4, "used_tokens": 9667, "context_window": null, "total_processed_tokens": null, "cost_usd": null, "duration_ms": null}),
                    json!({"type": "turn_completed", "turn_id": "opencode-turn-1", "status": "completed", "usage": {"freshness": "current", "turn_processed_tokens": 9667, "input_tokens": 6335, "cached_input_tokens": 3328, "output_tokens": 4, "used_tokens": 9667, "context_window": null, "total_processed_tokens": null, "cost_usd": 0.0, "duration_ms": null}}),
                ]
            );
            shut_down(handle).await;
            assert_eq!(
                fixture.server.writes(),
                vec![
                    (
                        "POST /api/session".into(),
                        json!({"model": {"providerID": "opencode", "id": "deepseek-v4.1-flash-free", "variant": "low"}})
                    ),
                    (
                        "PUT /api/experimental/session/ses_root/instructions/entries/tcode".into(),
                        json!({"value": "<pull_request_linking>\nLink every PR.\n</pull_request_linking>\n\n"})
                    ),
                    (
                        PROMPT.into(),
                        json!({"text": "Reply with exactly: PONG", "delivery": "queue"})
                    ),
                ]
            );
        });
    }

    #[test]
    fn a_shell_call_streams_its_output_and_reports_its_exit_code() {
        smol::block_on(async {
            const OUTPUT: &str = "GET /api/shell/sh_198ac475f001twGtiLgjWLI7iQ/output";
            let fixture = fixture(
                script(
                    "shell_turn.jsonl",
                    &[
                        ("session.inbox.enqueued", PROMPT, 1),
                        // Each page reaches the actor before the poller asks for the next.
                        ("shell.exited", &format!("{OUTPUT}?cursor=8"), 1),
                    ],
                ),
                vec![
                    (PROMPT, 200, inbox("msg_198ac3fd6001psMVlt0sBx0Lni")),
                    (
                        &format!("{OUTPUT}?cursor=0"),
                        200,
                        r#"{"location":{"directory":"/work/project"},"data":{"output":"one\n","cursor":4,"size":4,"truncated":false}}"#.into(),
                    ),
                    (
                        &format!("{OUTPUT}?cursor=4"),
                        200,
                        r#"{"location":{"directory":"/work/project"},"data":{"output":"two\n","cursor":8,"size":8,"truncated":false}}"#.into(),
                    ),
                ],
            );
            let handle = open(fixture.options.clone()).await;
            send(&handle, turn("Run echo one; sleep 6; echo two")).await;
            let id = "msg_198ac3fdc0015DUJhtvxNwLad4:chatcmpl-tool-91184514ce90f973";
            assert_eq!(
                items_only(until(&handle, turn_completed).await),
                vec![
                    json!({"type": "item_started", "id": id, "content": {"kind": "command_execution", "command": "", "output": "", "exit_code": null, "status": "in_progress"}}),
                    json!({"type": "item_updated", "id": id, "content": {"kind": "command_execution", "command": "echo one; sleep 6; echo two", "output": "", "exit_code": null, "status": "in_progress"}}),
                    json!({"type": "delta", "item_id": id, "kind": "command_output", "text": "one\n"}),
                    json!({"type": "delta", "item_id": id, "kind": "command_output", "text": "two\n"}),
                    json!({"type": "item_completed", "id": id, "content": {"kind": "command_execution", "command": "echo one; sleep 6; echo two", "output": "one\ntwo\n", "exit_code": 0, "status": "completed"}}),
                    json!({"type": "item_started", "id": "msg_198ac5ee50018mgQBTDX0Qzgxm:text:0", "content": {"kind": "assistant_message", "text": ""}}),
                    json!({"type": "delta", "item_id": "msg_198ac5ee50018mgQBTDX0Qzgxm:text:0", "kind": "assistant_text", "text": "DONE"}),
                    json!({"type": "item_completed", "id": "msg_198ac5ee50018mgQBTDX0Qzgxm:text:0", "content": {"kind": "assistant_message", "text": "DONE"}}),
                    json!({"type": "turn_completed", "turn_id": "opencode-turn-1", "status": "completed", "usage": {"freshness": "current", "turn_processed_tokens": 20463, "input_tokens": 125, "cached_input_tokens": 10112, "output_tokens": 4, "used_tokens": 10241, "context_window": null, "total_processed_tokens": null, "cost_usd": 0.0, "duration_ms": null}}),
                ]
            );
            shut_down(handle).await;
        });
    }

    #[test]
    fn file_tools_carry_their_own_diffs_todos_set_the_plan_and_the_turn_diff_is_exact() {
        smol::block_on(async {
            const DIFF: &str = "GET /api/session/ses_root/diff";
            let recorded_diff = r#"{"data":[{"file":"notes2.txt","patch":"diff --git a/notes2.txt b/notes2.txt\nnew file mode 100644\nindex 0000000..a5b73ed\n--- /dev/null\n+++ b/notes2.txt\n@@ -0,0 +1 @@\n+green\n","additions":1,"deletions":0,"status":"added"}]}"#;
            let fixture = fixture(
                script("files_turn.jsonl", &[("session.inbox.enqueued", PROMPT, 1)]),
                vec![
                    (PROMPT, 200, inbox("msg_198b17718001R9iMtgnHHDzIX7")),
                    (DIFF, 200, recorded_diff.into()),
                ],
            );
            let handle = open(fixture.options.clone()).await;
            send(&handle, turn("Record todos, create notes2.txt, edit it")).await;
            let write = "msg_198b17ffc001GbWFSCOl8PceDQ:chatcmpl-tool-900118863147ca5c";
            let edit = "msg_198b183de001BRg89zKzkJddrX:chatcmpl-tool-abdddfec25b4b5cb";
            let turn_diff = json!({"type": "turn_changes_updated", "turn_id": "opencode-turn-1", "changes": [{"path": "notes2.txt", "kind": "create", "diff": "diff --git a/notes2.txt b/notes2.txt\nnew file mode 100644\nindex 0000000..a5b73ed\n--- /dev/null\n+++ b/notes2.txt\n@@ -0,0 +1 @@\n+green\n"}], "completeness": "exact"});
            let mut events = items_only(until(&handle, turn_completed).await);
            events.pop();
            assert_eq!(
                events,
                vec![
                    json!({"type": "item_started", "id": "msg_198b17729001BfnSeHTUEw26zm:text:0", "content": {"kind": "assistant_message", "text": ""}}),
                    json!({"type": "delta", "item_id": "msg_198b17729001BfnSeHTUEw26zm:text:0", "kind": "assistant_text", "text": "I'll do these in order.\n\n"}),
                    json!({"type": "item_completed", "id": "msg_198b17729001BfnSeHTUEw26zm:text:0", "content": {"kind": "assistant_message", "text": "I'll do these in order.\n\n"}}),
                    json!({"type": "plan_updated", "turn_id": "opencode-turn-1", "explanation": null, "steps": [{"step": "Write code", "status": "inProgress"}, {"step": "Test code", "status": "pending"}]}),
                    json!({"type": "item_started", "id": write, "content": {"kind": "file_change", "changes": [], "status": "in_progress"}}),
                    json!({"type": "item_updated", "id": write, "content": {"kind": "file_change", "changes": [{"path": "/work/project/notes2.txt", "kind": "modify", "diff": null}], "status": "in_progress"}}),
                    json!({"type": "item_completed", "id": write, "content": {"kind": "file_change", "changes": [{"path": "/work/project/notes2.txt", "kind": "create", "diff": "+blue"}], "status": "completed"}}),
                    turn_diff.clone(),
                    json!({"type": "item_started", "id": edit, "content": {"kind": "file_change", "changes": [], "status": "in_progress"}}),
                    json!({"type": "item_updated", "id": edit, "content": {"kind": "file_change", "changes": [{"path": "/work/project/notes2.txt", "kind": "modify", "diff": null}], "status": "in_progress"}}),
                    json!({"type": "item_completed", "id": edit, "content": {"kind": "file_change", "changes": [{"path": "notes2.txt", "kind": "modify", "diff": "Index: notes2.txt\n===================================================================\n--- notes2.txt\n+++ notes2.txt\n@@ -1,1 +1,1 @@\n-blue\n+green\n"}], "status": "completed"}}),
                    turn_diff,
                    json!({"type": "item_started", "id": "msg_198b18977001VDeVULdbXIIY47:text:0", "content": {"kind": "assistant_message", "text": ""}}),
                    json!({"type": "delta", "item_id": "msg_198b18977001VDeVULdbXIIY47:text:0", "kind": "assistant_text", "text": "DONE"}),
                    json!({"type": "item_completed", "id": "msg_198b18977001VDeVULdbXIIY47:text:0", "content": {"kind": "assistant_message", "text": "DONE"}}),
                ]
            );
            shut_down(handle).await;
        });
    }

    #[test]
    fn a_subagents_activity_nests_under_the_call_that_spawned_it() {
        smol::block_on(async {
            let fixture = fixture(
                script(
                    "subagent_turn.jsonl",
                    &[("session.inbox.enqueued", PROMPT, 1)],
                ),
                vec![(PROMPT, 200, inbox("msg_198acc1d60017L93dqVLD4mt2r"))],
            );
            let handle = open(fixture.options.clone()).await;
            send(&handle, turn("Ask a subagent to reply KIWI")).await;
            let spawn = "msg_198acc1db0013V4MEKRcfmYvtt:chatcmpl-tool-b5f7774ecdb70d58";
            let child = "msg_198accaf2001JTZdDPBbxetEk2:text:0";
            let reply = "msg_198accd89001hG3ffj4MheRnb9:text:0";
            let events = until(&handle, turn_completed).await;
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["type"] == "turn_completed")
                    .count(),
                1,
                "the child's execution is not a turn of its own"
            );
            assert_eq!(
                items_only(events)[..9],
                [
                    json!({"type": "item_started", "id": spawn, "content": {"kind": "subagent", "agent_type": "", "description": "", "status": "in_progress", "summary": null}}),
                    json!({"type": "item_updated", "id": spawn, "content": {"kind": "subagent", "agent_type": "general", "description": "Reply with KIWI", "status": "in_progress", "summary": null}}),
                    json!({"type": "item_updated", "id": spawn, "content": {"kind": "subagent", "agent_type": "general", "description": "Reply with KIWI", "status": "in_progress", "summary": null, "model": "opencode/deepseek-v4.1-flash-free"}}),
                    json!({"type": "item_started", "id": child, "parent_item_id": spawn, "content": {"kind": "assistant_message", "text": ""}}),
                    json!({"type": "delta", "item_id": child, "kind": "assistant_text", "text": "KIWI"}),
                    json!({"type": "item_completed", "id": child, "parent_item_id": spawn, "content": {"kind": "assistant_message", "text": "KIWI"}}),
                    json!({"type": "item_completed", "id": spawn, "content": {"kind": "subagent", "agent_type": "general", "description": "Reply with KIWI", "status": "completed", "summary": "KIWI", "model": "opencode/deepseek-v4.1-flash-free"}}),
                    json!({"type": "item_started", "id": reply, "content": {"kind": "assistant_message", "text": ""}}),
                    json!({"type": "delta", "item_id": reply, "kind": "assistant_text", "text": "The subagent said: KIWI"}),
                ]
            );
            shut_down(handle).await;
        });
    }

    #[test]
    fn an_image_read_carries_the_image_it_read() {
        smol::block_on(async {
            let fixture = fixture(
                script("image_turn.jsonl", &[("session.inbox.enqueued", PROMPT, 1)]),
                vec![(PROMPT, 200, inbox("msg_198ad1762001jqZO7WLpW7kvOb"))],
            );
            let handle = open(fixture.options.clone()).await;
            send(&handle, turn("Read blue.png")).await;
            let read = "msg_198ad1767001iy9JUcC3DFxDk1:chatcmpl-tool-9abf3cdf50e4c7d9";
            assert_eq!(
                items_only(until(&handle, turn_completed).await)[..3],
                [
                    json!({"type": "item_started", "id": read, "content": {"kind": "tool_call", "name": "read", "input": {}, "output": null, "status": "in_progress"}}),
                    json!({"type": "item_updated", "id": read, "content": {"kind": "tool_call", "name": "read", "input": {"path": "blue.png"}, "output": null, "status": "in_progress"}}),
                    json!({"type": "item_completed", "id": read, "content": {"kind": "tool_call", "image_reads": [{"media_type": "image/png", "data_base64": "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEElEQVR4nGNgYPiPAw0pCQCpcD/BFMrqcwAAAABJRU5ErkJggg=="}], "name": "read", "input": {"path": "blue.png"}, "output": "Image read successfully", "status": "completed"}}),
                ]
            );
            shut_down(handle).await;
        });
    }

    #[test]
    fn an_mcp_call_names_its_server_and_tool_and_carries_its_text_and_images() {
        smol::block_on(async {
            let fixture = fixture(
                script("mcp_turn.jsonl", &[("session.inbox.enqueued", PROMPT, 1)]),
                vec![
                    (
                        "GET /api/mcp",
                        200,
                        r#"{"location":{"directory":"/work/project"},"data":[{"name":"tcode_probe","status":{"status":"connected"}}]}"#.into(),
                    ),
                    (PROMPT, 200, inbox("msg_198d2d16b001pV9qJ5Qo7JvzVe")),
                ],
            );
            let handle = open(fixture.options.clone()).await;
            send(&handle, turn("Call echo_upper with hello")).await;
            let call = "msg_198d2e71d001VpxRG5X5z2lMvh:chatcmpl-tool-a75c2f7c0101d202";
            let completed = until(&handle, |event| {
                matches!(event, AgentEvent::ItemCompleted(ThreadItem { id, .. }) if id == call)
            })
            .await;
            assert_eq!(
                completed.last(),
                Some(
                    &json!({"type": "item_completed", "id": call, "content": {"kind": "tool_call", "image_reads": [{"media_type": "image/png", "data_base64": "iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAIAAAAmkwkpAAAAEElEQVR4nGP4z8AARwzEcQCukw/x0F8jngAAAABJRU5ErkJggg=="}], "name": "tcode_probe/echo_upper", "input": {"text": "hello"}, "output": "HELLO", "status": "completed"}})
                )
            );
            shut_down(handle).await;
        });
    }

    #[test]
    fn permission_requests_offer_native_options_and_each_answer_reaches_the_asking_session() {
        const REQUEST: &str = "per_198adfe110010MaNqr8j5fFdKI";
        let reply = format!("POST /api/session/ses_root/permission/{REQUEST}/reply");
        let asked = json!({"type": "approval_requested", "id": REQUEST, "turn_id": "opencode-turn-1", "kind": {"kind": "exec_command", "command": "echo hi", "cwd": null, "reason": null}, "options": [
            {"id": "once", "label": "Allow once", "kind": "allow_once"},
            {"id": "always", "label": "Always allow", "kind": "allow_always"},
            {"id": "reject", "label": "Reject", "kind": "reject_once"},
        ]});
        for (mode, decision, body) in [
            (
                "normal",
                Some(ApprovalDecision::Option("once".into())),
                json!({"decision": "once"}),
            ),
            (
                "normal",
                Some(ApprovalDecision::Feedback("Use printf instead".into())),
                json!({"decision": "reject", "message": "Use printf instead"}),
            ),
            ("auto", None, json!({"decision": "once"})),
        ] {
            smol::block_on(async {
                let fixture = fixture(
                    script(
                        "permission_turn.jsonl",
                        &[
                            ("session.inbox.enqueued", PROMPT, 1),
                            ("permission.replied", &reply, 1),
                        ],
                    ),
                    vec![(PROMPT, 200, inbox("msg_198adf558001Nw9Cn3CZxbyQHB"))],
                );
                let mut options = fixture.options.clone();
                options.option_selections.push(OptionSelection {
                    id: PERMISSION_MODE.into(),
                    value: json!(mode),
                });
                let handle = open(options).await;
                send(&handle, turn("Run echo hi")).await;
                let events = until(&handle, |event| {
                    matches!(
                        event,
                        AgentEvent::ApprovalRequested(_) | AgentEvent::ApprovalResolved { .. }
                    )
                })
                .await;
                let resolved = match decision {
                    Some(decision) => {
                        assert_eq!(events.last(), Some(&asked));
                        send(
                            &handle,
                            SessionCommand::RespondApproval {
                                request_id: REQUEST.into(),
                                decision: decision.clone(),
                            },
                        )
                        .await;
                        until(&handle, |event| {
                            matches!(event, AgentEvent::ApprovalResolved { .. })
                        })
                        .await
                        .pop()
                        .unwrap()
                    }
                    None => events.last().cloned().unwrap(),
                };
                assert_eq!(resolved["type"], "approval_resolved");
                let rest = until(&handle, turn_completed).await;
                assert!(
                    !rest
                        .iter()
                        .any(|event| event["type"] == "approval_resolved"),
                    "OpenCode's own echo of the reply resolves nothing twice"
                );
                shut_down(handle).await;
                let replies: Vec<Value> = fixture
                    .server
                    .writes()
                    .into_iter()
                    .filter(|(key, _)| *key == reply)
                    .map(|(_, body)| body)
                    .collect();
                assert_eq!(replies, vec![body], "{mode}");
            });
        }
    }

    #[test]
    fn a_question_arrives_as_a_form_and_the_answer_replies_to_it() {
        smol::block_on(async {
            const FORM: &str = "frm_198ad5065001DTwIUpRmyj3Mio";
            let reply = format!("POST /api/session/ses_root/form/{FORM}/reply");
            let fixture = fixture(
                script(
                    "question_turn.jsonl",
                    &[
                        ("session.inbox.enqueued", PROMPT, 1),
                        ("form.replied", &reply, 1),
                    ],
                ),
                vec![(PROMPT, 200, inbox("msg_198ad4c4f001xLFAPnNiNafuXE"))],
            );
            let handle = open(fixture.options.clone()).await;
            send(&handle, turn("Ask me which color I prefer")).await;
            let asked = until(&handle, |event| {
                matches!(event, AgentEvent::UserInputRequested { .. })
            })
            .await;
            assert_eq!(
                asked.last(),
                Some(
                    &json!({"type": "user_input_requested", "request_id": FORM, "questions": [{
                        "id": "q0", "header": "Preferred color", "question": "Which color do you prefer?",
                        "options": [{"label": "Red", "description": "Prefer red"}, {"label": "Blue", "description": "Prefer blue"}],
                        "multi_select": false,
                    }]})
                )
            );
            let answers = Map::from_iter([("q0".to_owned(), json!("Blue"))]);
            send(
                &handle,
                SessionCommand::RespondUserInput {
                    request_id: FORM.into(),
                    answers,
                },
            )
            .await;
            let rest = until(&handle, turn_completed).await;
            assert_eq!(
                rest.iter()
                    .filter(|event| event["type"] == "user_input_resolved")
                    .collect::<Vec<_>>(),
                [
                    &json!({"type": "user_input_resolved", "request_id": FORM, "answers": {"q0": "Blue"}})
                ]
            );
            shut_down(handle).await;
            assert!(
                fixture
                    .server
                    .writes()
                    .contains(&(reply, json!({"answer": {"q0": "Blue"}})))
            );
        });
    }

    #[test]
    fn a_steer_joins_the_running_turn_once_opencode_delivers_it() {
        smol::block_on(async {
            let fixture = fixture(
                script(
                    "steer_turn.jsonl",
                    &[
                        ("session.inbox.enqueued", PROMPT, 1),
                        ("session.inbox.enqueued", PROMPT, 2),
                    ],
                ),
                vec![
                    (PROMPT, 200, inbox("msg_198ae70f20011oI4C5n83g0YJ4")),
                    (PROMPT, 200, inbox("msg_198ae7cda001l18XtdGE8aDbE0")),
                ],
            );
            let handle = open(fixture.options.clone()).await;
            send(&handle, turn("Run sleep 6, then reply DONE")).await;
            until(&handle, |event| matches!(event, AgentEvent::ItemUpdated(_))).await;
            send(
                &handle,
                SessionCommand::Steer {
                    request_id: "steer-1".into(),
                    text: "Ignore the earlier request and reply PONG only.".into(),
                    attachments: Vec::new(),
                },
            )
            .await;
            let events = until(&handle, turn_completed).await;
            let accepted = events
                .iter()
                .position(|event| {
                    *event == json!({"type": "steer_accepted", "request_id": "steer-1"})
                })
                .expect("the delivered steer is accepted");
            let reply = events
                .iter()
                .position(|event| event["type"] == "delta" && event["text"] == "PONG")
                .unwrap();
            assert!(accepted < reply, "accepted before the reply it caused");
            shut_down(handle).await;
            assert_eq!(
                fixture.server.writes()[3],
                (
                    PROMPT.into(),
                    json!({"text": "Ignore the earlier request and reply PONG only.", "delivery": "steer"})
                )
            );
        });
    }

    #[test]
    fn an_interrupt_stops_the_running_call_and_turn() {
        smol::block_on(async {
            let interrupt = "POST /api/session/ses_root/interrupt";
            let fixture = fixture(
                script(
                    "interrupt_turn.jsonl",
                    &[
                        ("session.inbox.enqueued", PROMPT, 1),
                        ("shell.exited", interrupt, 1),
                    ],
                ),
                vec![
                    (PROMPT, 200, inbox("msg_198ae3e26001hGwkRovRB1DaDH")),
                    (interrupt, 200, r#"{"interrupted":true}"#.into()),
                ],
            );
            let handle = open(fixture.options.clone()).await;
            send(&handle, turn("Run sleep 30")).await;
            until(&handle, |event| {
                matches!(event, AgentEvent::ItemUpdated(ThreadItem { content: ItemContent::CommandExecution { command, .. }, .. }) if command == "sleep 30")
            })
            .await;
            send(&handle, SessionCommand::Interrupt).await;
            let id = "msg_198ae3e2d001J0cOFv8lV738Bg:chatcmpl-tool-be126f07a96b3c6d";
            assert_eq!(
                until(&handle, turn_completed).await,
                vec![
                    json!({"type": "item_completed", "id": id, "content": {"kind": "command_execution", "command": "sleep 30", "output": "", "exit_code": null, "status": "interrupted"}}),
                    json!({"type": "turn_completed", "turn_id": "opencode-turn-1", "status": "interrupted", "usage": null}),
                ]
            );
            shut_down(handle).await;
        });
    }

    #[test]
    fn a_rewind_reverts_the_conversation_and_files_to_its_prompt() {
        smol::block_on(async {
            const PROMPT_ID: &str = "msg_198aac9f2001K4cb8mDh24jTzH";
            let fixture = fixture(
                script("text_turn.jsonl", &[("session.inbox.enqueued", PROMPT, 1)]),
                vec![
                    (PROMPT, 200, inbox(PROMPT_ID)),
                    (
                        &format!("GET /api/session/ses_root/message/{PROMPT_ID}"),
                        200,
                        format!(
                            r#"{{"data":{{"type":"user","id":"{PROMPT_ID}","text":"Reply with exactly: PONG","time":{{"created":1793562692083}}}}}}"#
                        ),
                    ),
                    (
                        "POST /api/session/ses_root/revert/stage",
                        200,
                        format!(r#"{{"data":{{"messageID":"{PROMPT_ID}","files":[]}}}}"#),
                    ),
                ],
            );
            let handle = open(fixture.options.clone()).await;
            send(&handle, turn("Reply with exactly: PONG")).await;
            until(&handle, turn_completed).await;
            send(
                &handle,
                SessionCommand::Rewind {
                    checkpoint_id: PROMPT_ID.into(),
                    mode: RewindMode::FilesAndConversation,
                },
            )
            .await;
            assert_eq!(
                serde_json::to_value(recv(&handle).await).unwrap(),
                json!({"type": "rewind_completed", "checkpoint_id": PROMPT_ID, "mode": "files_and_conversation", "prefill": "Reply with exactly: PONG"})
            );
            shut_down(handle).await;
            assert_eq!(
                fixture.server.writes()[3..],
                [
                    (
                        "POST /api/session/ses_root/revert/stage".into(),
                        json!({"messageID": PROMPT_ID, "files": true})
                    ),
                    ("POST /api/session/ses_root/revert/commit".into(), json!({})),
                ]
            );
        });
    }

    #[test]
    fn a_manual_compaction_runs_natively_and_reports_its_progress() {
        smol::block_on(async {
            let compact = "POST /api/session/ses_root/compact";
            let fixture = fixture(
                script(
                    "compact_turn.jsonl",
                    &[("session.inbox.enqueued", compact, 1)],
                ),
                Vec::new(),
            );
            let handle = open(fixture.options.clone()).await;
            send(&handle, SessionCommand::Compact).await;
            assert_eq!(
                until(&handle, turn_completed).await,
                vec![
                    json!({"type": "turn_started", "turn_id": "opencode-turn-1"}),
                    json!({"type": "context_compacted", "in_progress": true, "trigger": "manual", "pre_tokens": null, "post_tokens": null, "dropped_tokens": null, "duration_ms": null}),
                    json!({"type": "context_compacted", "in_progress": false, "trigger": "manual", "pre_tokens": null, "post_tokens": null, "dropped_tokens": null, "duration_ms": null}),
                    json!({"type": "turn_completed", "turn_id": "opencode-turn-1", "status": "completed", "usage": null}),
                ]
            );
            shut_down(handle).await;
            assert_eq!(
                fixture.server.writes()[1..],
                [
                    // Without instructions, an entry an earlier start left is removed.
                    (
                        "DELETE /api/experimental/session/ses_root/instructions/entries/tcode"
                            .into(),
                        Value::Null
                    ),
                    (compact.into(), json!({})),
                ]
            );
        });
    }

    #[test]
    fn commands_run_through_their_route_and_skill_mentions_attach_the_skill() {
        smol::block_on(async {
            let commands = r#"{"location":{"directory":"/work/project"},"data":[{"name":"review","description":"review changes [commit|branch|pr], defaults to uncommitted"}]}"#;
            let skills = r##"{"location":{"directory":"/work/project"},"data":[{"id":"opencode","name":"OpenCode","description":"Use this skill for any question about OpenCode itself.","path":"/builtin/opencode.md","content":"# OpenCode"}]}"##;
            let fixture = fixture(
                vec![Step::Frame(
                    serde_json::from_str(r#"{"id":"evt_198b0ff530020lZiK9TNv08sop","created":1793563098963,"type":"command.updated","location":{"directory":"/work/project"},"data":{}}"#).unwrap(),
                )],
                vec![
                    // Plugins register commands only after the session opened.
                    ("GET /api/command", 200, r#"{"data":[]}"#.into()),
                    ("GET /api/command", 200, commands.into()),
                    ("GET /api/skill", 200, skills.into()),
                    (PROMPT, 200, inbox("msg_198ae70f20011oI4C5n83g0YJ4")),
                ],
            );
            let handle = open(fixture.options.clone()).await;
            let review = json!({"name": "review", "description": "review changes [commit|branch|pr], defaults to uncommitted", "kind": "command"});
            let skill = json!({"name": "opencode", "description": "Use this skill for any question about OpenCode itself.", "kind": "skill"});
            assert_eq!(
                serde_json::to_value(recv(&handle).await).unwrap(),
                json!({"type": "provider_commands", "commands": [review, skill]})
            );
            send(&handle, turn("/review the last commit")).await;
            send(&handle, turn("$opencode how do agents work?")).await;
            recv(&handle).await;
            recv(&handle).await;
            shut_down(handle).await;
            assert_eq!(
                fixture.server.writes()[2..],
                [
                    (
                        "POST /api/session/ses_root/command".into(),
                        json!({"name": "review", "text": "the last commit", "delivery": "queue"})
                    ),
                    (
                        PROMPT.into(),
                        json!({"text": "$opencode how do agents work?", "delivery": "queue", "skills": [{"id": "opencode", "mention": {"start": 0, "end": 9, "text": "$opencode"}}]})
                    ),
                ]
            );
        });
    }

    #[test]
    fn an_effort_change_switches_the_session_models_variant_before_the_prompt() {
        smol::block_on(async {
            let fixture = fixture(
                Vec::new(),
                vec![(PROMPT, 200, inbox("msg_198aac9f2001K4cb8mDh24jTzH"))],
            );
            let handle = open(fixture.options.clone()).await;
            for _ in 0..2 {
                send(
                    &handle,
                    SessionCommand::SendTurn {
                        delivery_id: 1,
                        text: "Think harder".into(),
                        options: Some(TurnOptions {
                            effort: Some("max".into()),
                        }),
                        attachments: Vec::new(),
                    },
                )
                .await;
                recv(&handle).await;
            }
            shut_down(handle).await;
            assert_eq!(
                fixture.server.writes()[2..],
                [
                    (
                        "POST /api/session/ses_root/model".into(),
                        json!({"model": {"providerID": "opencode", "id": "deepseek-v4.1-flash-free", "variant": "max"}})
                    ),
                    (
                        PROMPT.into(),
                        json!({"text": "Think harder", "delivery": "queue"})
                    ),
                    (
                        PROMPT.into(),
                        json!({"text": "Think harder", "delivery": "queue"})
                    ),
                ]
            );
        });
    }

    #[test]
    fn tcode_mcp_servers_join_the_inline_config_as_tools_of_their_own() {
        smol::block_on(async {
            let fixture = fixture(Vec::new(), Vec::new());
            let mut options = fixture.options.clone();
            options.launch_env.env.push((
                "OPENCODE_CONFIG_CONTENT".into(),
                r#"{"experimental":{"todo":true},"mcp":{"servers":{"docs":{"type":"local","command":["docs-mcp"]}}}}"#.into(),
            ));
            options.mcp_servers = vec![McpRegistration {
                name: McpRegistration::SERVER_NAME_PREVIEW.into(),
                url: "http://127.0.0.1:53211/mcp".into(),
                bearer_token: "token".into(),
            }];
            let handle = open(options).await;
            shut_down(handle).await;
            let config: Value = serde_json::from_str(
                &std::fs::read_to_string(fixture.directory.join("config")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                config,
                json!({"experimental": {"todo": true}, "mcp": {"servers": {
                    "docs": {"type": "local", "command": ["docs-mcp"]},
                    "tcode_preview": {"type": "remote", "url": "http://127.0.0.1:53211/mcp", "headers": {"Authorization": "Bearer token"}, "codemode": false},
                }}})
            );
        });
    }

    #[test]
    fn the_model_catalog_offers_each_model_with_its_variants_in_opencodes_order() {
        smol::block_on(async {
            let fixture = fixture(
                Vec::new(),
                vec![
                    ("GET /api/model", 200, fixture_text("model_list.json")),
                    (
                        "GET /api/model/default",
                        200,
                        fixture_text("model_default.json"),
                    ),
                ],
            );
            let models = list_models(
                fixture.options.binary_path.clone(),
                fixture.options.launch_env.clone(),
            )
            .await
            .unwrap();
            let effort = |variants: &[&str]| json!([{"kind": "select", "id": "reasoningEffort", "label": "Thinking", "options": variants.iter().map(|variant| json!({"value": variant, "label": title_case(variant), "description": null, "unavailable": null})).collect::<Vec<_>>(), "default_value": null, "role": "model", "apply": "live", "recommended": null, "permissive": null}]);
            assert_eq!(
                serde_json::to_value(&models).unwrap(),
                json!([
                    {"id": "openai/gpt-6.1-terra", "display_name": "GPT-6.1 Terra", "is_default": true, "options": effort(&["none", "low", "medium", "high", "xhigh", "max"])},
                    {"id": "opencode/big-pickle", "display_name": "Big Pickle", "is_default": false, "options": []},
                ])
            );
        });
    }
}
