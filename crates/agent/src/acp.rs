//! Agent Client Protocol provider: any agent from the ACP registry.
//!
//! Codex and Claude Code keep their native clients (they expose steering,
//! structured questions and richer tool payloads that ACP cannot carry); this
//! module covers the rest of the ecosystem through one protocol.
//!
//! Shape: one child process per session, JSON-RPC over its stdio, driven by
//! `agent_client_protocol::ClientSideConnection`. The protocol crate's `Client`
//! trait is `?Send` (its futures are `!Send`), so the whole connection lives on
//! a dedicated thread running a `LocalExecutor`; the canonical [`AgentEvent`]
//! stream and the [`SessionCommand`] channel are the only things that cross the
//! thread boundary — exactly like `claude.rs` / `codex.rs`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use acp::{Agent as _, Client};
use agent_client_protocol as acp;
use async_channel::{Receiver, Sender};
use futures_lite::{AsyncBufReadExt as _, AsyncReadExt as _, StreamExt as _, future};
use serde_json::{Value, json};

use crate::{
    AcpAgent, AcpLaunch, AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption,
    ApprovalOptionKind, ApprovalRequest, Attachment, DeltaKind, FileChange, FileChangeKind,
    ItemContent, ItemStatus, McpRegistration, OptionDescriptor, OptionSelection, PlanStep,
    PlanStepStatus, ProviderCommand, ProviderCommandKind, ProviderKind, ResumeCursor, SelectOption,
    SessionCommand, SessionHandle, SessionOptions, ThreadItem, TokenUsage, TurnStatus,
};

/// Option-descriptor ids. The composer renders an ACP agent's own
/// modes/models/config options through the existing traits picker, so each one
/// needs a stable id that routes back to the right ACP method.
const MODE_OPTION_ID: &str = "acp:mode";
const MODEL_OPTION_ID: &str = "acp:model";
const CONFIG_OPTION_PREFIX: &str = "acp:cfg:";

/// Cap on captured terminal output when the agent sets none (1 MiB).
const DEFAULT_TERMINAL_OUTPUT_LIMIT: u64 = 1 << 20;

/// How long we wait for an agent to answer `initialize` before declaring it
/// broken. Generous: an `npm exec` recipe may have to fetch the package first.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long we let an agent's `authenticate` run before giving up and telling
/// the user to sign in with the agent's own CLI (Gemini's is a browser OAuth
/// flow that otherwise blocks session startup for five minutes).
const AUTH_TIMEOUT: Duration = Duration::from_secs(20);

/// Start (or resume) a session with an ACP agent.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    let (commands_tx, commands_rx) = async_channel::unbounded();
    let (events_tx, events_rx) = async_channel::unbounded();
    let (ready_tx, ready_rx) = async_channel::bounded(1);

    // A dedicated thread with a single-threaded executor: `acp::Client` is
    // `#[async_trait(?Send)]`, so none of the connection's futures are `Send`
    // and they cannot ride on smol's global (work-stealing) executor.
    std::thread::Builder::new()
        .name("acp-session".into())
        .spawn(move || {
            let executor = Rc::new(smol::LocalExecutor::new());
            let task = run_actor(executor.clone(), opts, commands_rx, events_tx, ready_tx);
            futures_lite::future::block_on(executor.run(task));
        })
        .map_err(|err| {
            AgentError::Spawn(format!("could not start the ACP session thread: {err}"))
        })?;

    ready_rx.recv().await.map_err(|_| {
        AgentError::Protocol("ACP actor exited before reporting startup status".into())
    })??;

    Ok(SessionHandle {
        provider: ProviderKind::Acp,
        commands: commands_tx,
        events: events_rx,
    })
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

/// The resolved command line for a launch recipe.
///
/// `Npx` becomes `npm exec --yes -- <package> <args…>` (the registry's own
/// contract, and what zed runs); `Binary` / `Custom` run as given.
pub fn launch_command(launch: &AcpLaunch) -> Result<(PathBuf, Vec<String>), AgentError> {
    match launch {
        AcpLaunch::Npx { package, args, .. } => {
            let npm = crate::resolve_binary(None, "npm")?;
            let mut argv = vec![
                "exec".to_string(),
                "--yes".to_string(),
                "--".to_string(),
                package.clone(),
            ];
            argv.extend(args.iter().cloned());
            Ok((npm, argv))
        }
        AcpLaunch::Binary { command, args, .. } => Ok((command.clone(), args.clone())),
        AcpLaunch::Custom { command, args, .. } => {
            let binary = crate::resolve_binary(None, command)?;
            Ok((binary, args.clone()))
        }
    }
}

/// Environment pairs baked into the launch recipe (the registry's `env`).
fn recipe_env(launch: &AcpLaunch) -> &[(String, String)] {
    match launch {
        AcpLaunch::Npx { env, .. }
        | AcpLaunch::Binary { env, .. }
        | AcpLaunch::Custom { env, .. } => env,
    }
}

fn spawn_agent(
    agent: &AcpAgent,
    opts: &SessionOptions,
) -> Result<smol::process::Child, AgentError> {
    let (program, mut args) = launch_command(&agent.launch)?;
    args.extend(opts.extra_args.iter().cloned());

    let mut cmd = crate::process::async_command(&program);
    cmd.args(&args)
        .current_dir(&opts.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Recipe env first, the user's configured env last (the user always wins).
    for (key, value) in recipe_env(&agent.launch) {
        cmd.env(key, value);
    }
    for (key, value) in opts.launch_env.pairs(ProviderKind::Acp) {
        cmd.env(key, value);
    }
    log::debug!(
        "spawning ACP agent {}: {} {:?}",
        agent.id,
        program.display(),
        args
    );
    cmd.spawn().map_err(|err| {
        AgentError::Spawn(format!(
            "could not launch ACP agent `{}` ({}): {err}",
            agent.name,
            program.display()
        ))
    })
}

// ---------------------------------------------------------------------------
// Actor
// ---------------------------------------------------------------------------

async fn run_actor(
    executor: Rc<smol::LocalExecutor<'static>>,
    opts: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<AgentEvent>,
    ready: Sender<Result<(), AgentError>>,
) {
    let Some(agent) = opts.acp.clone() else {
        let _ = ready
            .send(Err(AgentError::Protocol(
                "no ACP agent selected for this session".into(),
            )))
            .await;
        return;
    };

    let mut child = match spawn_agent(&agent, &opts) {
        Ok(child) => child,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    // The agent's stderr is its log channel: keep the tail so a startup failure
    // can be reported in the agent's own words.
    let stderr_tail: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    executor
        .spawn({
            let tail = stderr_tail.clone();
            let id = agent.id.clone();
            async move {
                let mut lines = futures_lite::io::BufReader::new(stderr).lines();
                while let Some(Ok(line)) = lines.next().await {
                    log::debug!("acp[{id}] stderr: {line}");
                    let mut tail = tail.borrow_mut();
                    if tail.len() == 20 {
                        tail.remove(0);
                    }
                    tail.push(line);
                }
            }
        })
        .detach();

    let state = Rc::new(RefCell::new(State::new(opts.cwd.clone())));
    let client = AcpClient {
        events: events.clone(),
        state: state.clone(),
        cwd: opts.cwd.clone(),
        executor: executor.clone(),
    };

    let (connection, io_task) = acp::ClientSideConnection::new(client, stdin, stdout, {
        let executor = executor.clone();
        move |fut| executor.spawn(fut).detach()
    });
    let connection = Rc::new(connection);
    let (io_done_tx, io_done) = async_channel::bounded::<String>(1);
    executor
        .spawn(async move {
            let reason = match io_task.await {
                Ok(()) => "the ACP agent closed its stdio".to_string(),
                Err(err) => format!("ACP transport error: {err}"),
            };
            let _ = io_done_tx.send(reason).await;
        })
        .detach();

    // --- handshake ---------------------------------------------------------
    let session = match handshake(&connection, &agent, &opts, &state, &events).await {
        Ok(session) => session,
        Err(err) => {
            let tail = stderr_tail.borrow().join("\n");
            let err = match (&err, tail.trim().is_empty()) {
                (AgentError::Protocol(message), false) => {
                    AgentError::Protocol(format!("{message}\n{tail}"))
                }
                _ => err,
            };
            let _ = ready.send(Err(err)).await;
            let _ = child.kill();
            return;
        }
    };

    let _ = events
        .send(AgentEvent::SessionStarted {
            provider_session_id: session.session_id.0.to_string(),
            resume: ResumeCursor(json!({
                "acp_session_id": session.session_id.0.to_string(),
                "acp_agent_id": agent.id,
            })),
            model: session.model.clone(),
        })
        .await;
    emit_provider_options(&state, &events).await;
    if ready.send(Ok(())).await.is_err() {
        let _ = child.kill();
        return;
    }

    // --- command loop ------------------------------------------------------
    let (turn_tx, turn_done) = async_channel::unbounded::<TurnOutcome>();
    let mut turn_id: Option<String> = None;
    let mut turn_seq: u64 = 0;

    let close_reason = loop {
        enum Input {
            Command(Result<SessionCommand, async_channel::RecvError>),
            Turn(TurnOutcome),
            Io(String),
        }
        let input = future::or(
            future::or(async { Input::Command(commands.recv().await) }, async {
                match turn_done.recv().await {
                    Ok(outcome) => Input::Turn(outcome),
                    // This loop holds the sender, so the channel cannot close.
                    Err(_) => future::pending().await,
                }
            }),
            async {
                match io_done.recv().await {
                    Ok(reason) => Input::Io(reason),
                    Err(_) => future::pending().await,
                }
            },
        )
        .await;

        match input {
            Input::Command(Ok(SessionCommand::Shutdown)) | Input::Command(Err(_)) => break None,
            Input::Command(Ok(command)) => {
                handle_command(
                    command,
                    &executor,
                    &connection,
                    &session,
                    &state,
                    &events,
                    &turn_tx,
                    &mut turn_id,
                    &mut turn_seq,
                )
                .await;
            }
            Input::Turn(outcome) => {
                let id = turn_id.take().unwrap_or_else(|| outcome.turn_id.clone());
                finish_turn(&state, &events, &id, outcome).await;
            }
            Input::Io(reason) => break Some(reason),
        }
    };

    // Best-effort graceful close, then make sure the child is gone.
    if session.can_close {
        let _ = connection
            .close_session(acp::CloseSessionRequest::new(session.session_id.clone()))
            .await;
    }
    let _ = child.kill();
    let _ = events
        .send(AgentEvent::SessionClosed {
            reason: close_reason,
        })
        .await;
}

/// What `session/new` (or `session/load`) settled on.
struct Session {
    session_id: acp::SessionId,
    model: Option<String>,
    can_close: bool,
}

struct TurnOutcome {
    turn_id: String,
    result: Result<acp::PromptResponse, acp::Error>,
}

async fn handshake(
    connection: &Rc<acp::ClientSideConnection>,
    agent: &AcpAgent,
    opts: &SessionOptions,
    state: &Rc<RefCell<State>>,
    events: &Sender<AgentEvent>,
) -> Result<Session, AgentError> {
    // On a leash: an agent that starts but never answers `initialize` (cline
    // 3.0.39 does exactly this) would otherwise hang session startup forever,
    // with the UI stuck on "Starting…".
    let init = future::or(
        async {
            Some(
                connection
                    .initialize(
                        acp::InitializeRequest::new(acp::ProtocolVersion::LATEST)
                            .client_capabilities(
                                acp::ClientCapabilities::new()
                                    .fs(acp::FileSystemCapabilities::new()
                                        .read_text_file(true)
                                        .write_text_file(true))
                                    .terminal(true),
                            )
                            .client_info(
                                acp::Implementation::new("tcode", env!("CARGO_PKG_VERSION"))
                                    .title("tcode"),
                            ),
                    )
                    .await,
            )
        },
        async {
            smol::Timer::after(INITIALIZE_TIMEOUT).await;
            None
        },
    )
    .await;
    let init = match init {
        Some(Ok(init)) => init,
        Some(Err(err)) => {
            return Err(AgentError::Protocol(format!(
                "`{}` failed to initialize: {}",
                agent.name,
                describe(&err)
            )));
        }
        None => {
            return Err(AgentError::Protocol(format!(
                "`{}` did not answer `initialize` within {}s — it may not speak ACP over stdio (check its launch arguments)",
                agent.name,
                INITIALIZE_TIMEOUT.as_secs()
            )));
        }
    };

    let caps = init.agent_capabilities.clone();
    // Capability gate: our preview MCP server is a loopback streamable-HTTP
    // endpoint, so it may only be offered to agents that speak MCP over HTTP.
    let mcp_servers = mcp_servers(opts.mcp_server.as_ref(), &caps);
    if opts.mcp_server.is_some() && mcp_servers.is_empty() {
        log::info!(
            "acp[{}]: no mcpCapabilities.http; the preview MCP server is not registered",
            agent.id
        );
        let _ = events
            .send(AgentEvent::Warning(format!(
                "{} does not support HTTP MCP servers; the preview-browser tools are unavailable in this session",
                agent.name
            )))
            .await;
    }

    let resumed = opts
        .resume
        .as_ref()
        .and_then(|cursor| cursor.0.get("acp_session_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|_| caps.load_session);

    let (session_id, modes, models, config_options) = match resumed {
        Some(session_id) => {
            // `session/load` replays the whole conversation as `session/update`
            // notifications. Our JSONL log is the authoritative history and the
            // UI has already folded it, so the replay is swallowed (see
            // `State::replaying`); we only want the session live again.
            state.borrow_mut().replaying = true;
            let session_id = acp::SessionId::new(session_id);
            let loaded = connection
                .load_session(
                    acp::LoadSessionRequest::new(session_id.clone(), opts.cwd.clone())
                        .mcp_servers(mcp_servers.clone()),
                )
                .await;
            state.borrow_mut().replaying = false;
            match loaded {
                Ok(loaded) => (
                    session_id,
                    loaded.modes,
                    loaded.models,
                    loaded.config_options,
                ),
                Err(err) => {
                    log::warn!(
                        "acp[{}]: session/load failed ({}); starting a fresh session",
                        agent.id,
                        describe(&err)
                    );
                    let _ = events
                        .send(AgentEvent::Warning(format!(
                            "{} could not resume the previous conversation; starting a new one",
                            agent.name
                        )))
                        .await;
                    let new = new_session(connection, agent, opts, &mcp_servers, &init).await?;
                    (new.session_id, new.modes, new.models, new.config_options)
                }
            }
        }
        None => {
            let new = new_session(connection, agent, opts, &mcp_servers, &init).await?;
            (new.session_id, new.modes, new.models, new.config_options)
        }
    };

    let model = models
        .as_ref()
        .map(|models| models.current_model_id.0.to_string());
    {
        let mut state = state.borrow_mut();
        state.session_id = Some(session_id.clone());
        state
            .options
            .ingest(modes.as_ref(), models.as_ref(), config_options.as_deref());
    }

    Ok(Session {
        session_id,
        model,
        can_close: caps.session_capabilities.close.is_some(),
    })
}

async fn new_session(
    connection: &Rc<acp::ClientSideConnection>,
    agent: &AcpAgent,
    opts: &SessionOptions,
    mcp_servers: &[acp::McpServer],
    init: &acp::InitializeResponse,
) -> Result<acp::NewSessionResponse, AgentError> {
    let request =
        || acp::NewSessionRequest::new(opts.cwd.clone()).mcp_servers(mcp_servers.to_vec());
    match connection.new_session(request()).await {
        Ok(response) => Ok(response),
        Err(err) if is_auth_required(&err) => {
            // The agent wants credentials. Try its own `authenticate` once —
            // but on a leash: several agents (Gemini) implement it as an
            // interactive browser OAuth flow that blocks for minutes, and we
            // have no auth UI to show meanwhile. On timeout (or failure) we
            // surface a clear error naming the methods the agent offers.
            let Some(method) = preferred_auth_method(&init.auth_methods) else {
                return Err(AgentError::Provider(auth_hint(agent, init)));
            };
            let method_id = auth_method_id(&method);
            log::info!(
                "acp[{}]: session/new needs auth; trying method `{}`",
                agent.id,
                method_id.0
            );
            let authenticated = future::or(
                async {
                    Some(
                        connection
                            .authenticate(acp::AuthenticateRequest::new(method_id.clone()))
                            .await,
                    )
                },
                async {
                    smol::Timer::after(AUTH_TIMEOUT).await;
                    None
                },
            )
            .await;
            match authenticated {
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    return Err(AgentError::Provider(format!(
                        "{} (authentication via `{}` failed: {})",
                        auth_hint(agent, init),
                        method_id.0,
                        describe(&err)
                    )));
                }
                None => {
                    return Err(AgentError::Provider(format!(
                        "{} (its `{}` flow did not complete within {}s — finish it in the agent's own CLI first)",
                        auth_hint(agent, init),
                        method_id.0,
                        AUTH_TIMEOUT.as_secs()
                    )));
                }
            }
            connection.new_session(request()).await.map_err(|err| {
                if is_auth_required(&err) {
                    AgentError::Provider(auth_hint(agent, init))
                } else {
                    AgentError::Protocol(format!(
                        "`{}` could not start a session: {}",
                        agent.name,
                        describe(&err)
                    ))
                }
            })
        }
        Err(err) => Err(AgentError::Protocol(format!(
            "`{}` could not start a session: {}",
            agent.name,
            describe(&err)
        ))),
    }
}

/// Which auth method to drive over the protocol: an `env_var` method first (it
/// only validates variables we have already injected, so it is cheap and
/// non-interactive), otherwise the agent's first choice.
fn preferred_auth_method(methods: &[acp::AuthMethod]) -> Option<acp::AuthMethod> {
    methods
        .iter()
        .find(|method| matches!(method, acp::AuthMethod::EnvVar(_)))
        .or_else(|| methods.first())
        .cloned()
}

fn auth_method_id(method: &acp::AuthMethod) -> acp::AuthMethodId {
    match method {
        acp::AuthMethod::Agent(method) => method.id.clone(),
        acp::AuthMethod::EnvVar(method) => method.id.clone(),
        acp::AuthMethod::Terminal(method) => method.id.clone(),
        _ => acp::AuthMethodId::new("default"),
    }
}

/// The message shown when an agent demands credentials we cannot supply.
pub(crate) fn auth_hint(agent: &AcpAgent, init: &acp::InitializeResponse) -> String {
    let methods: Vec<String> = init
        .auth_methods
        .iter()
        .map(|method| match method {
            acp::AuthMethod::Agent(method) => method.name.clone(),
            acp::AuthMethod::EnvVar(method) => format!(
                "{} ({})",
                method.name,
                method
                    .vars
                    .iter()
                    .map(|var| var.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            acp::AuthMethod::Terminal(method) => method.name.clone(),
            _ => "unknown".to_string(),
        })
        .collect();
    let offered = if methods.is_empty() {
        String::new()
    } else {
        format!(" It offers: {}.", methods.join("; "))
    };
    format!(
        "`{}` requires authentication.{offered} Sign in with the agent's own CLI, or set its API-key environment variables in Settings → Providers → ACP Agents → {}.",
        agent.name, agent.name
    )
}

fn is_auth_required(err: &acp::Error) -> bool {
    i32::from(err.code) == -32000
}

fn describe(err: &acp::Error) -> String {
    match &err.data {
        Some(data) => format!("{} ({data})", err.message),
        None => err.message.clone(),
    }
}

/// The `mcpServers` array for `session/new` — our preview server, but only when
/// the agent advertises `mcpCapabilities.http`.
pub(crate) fn mcp_servers(
    registration: Option<&McpRegistration>,
    caps: &acp::AgentCapabilities,
) -> Vec<acp::McpServer> {
    match registration {
        Some(mcp) if caps.mcp_capabilities.http => vec![acp::McpServer::Http(
            acp::McpServerHttp::new(McpRegistration::SERVER_NAME, mcp.url.clone()).headers(vec![
                acp::HttpHeader::new("Authorization", format!("Bearer {}", mcp.bearer_token)),
            ]),
        )],
        _ => Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    command: SessionCommand,
    executor: &Rc<smol::LocalExecutor<'static>>,
    connection: &Rc<acp::ClientSideConnection>,
    session: &Session,
    state: &Rc<RefCell<State>>,
    events: &Sender<AgentEvent>,
    turn_tx: &Sender<TurnOutcome>,
    turn_id: &mut Option<String>,
    turn_seq: &mut u64,
) {
    match command {
        // ACP has no steering method at all (`session/prompt` is one request per
        // turn; only `session/cancel` interrupts). The app gates the steer button
        // off for ACP sessions and queues instead — this arm exists so a stray
        // command cannot hang the turn.
        SessionCommand::Steer { .. } => {
            log::warn!("acp: steering is not part of the protocol; ignoring");
            let _ = events
                .send(AgentEvent::Warning(
                    "This agent cannot be steered (ACP has no steering method); \
                     send the message after the turn finishes."
                        .into(),
                ))
                .await;
        }
        SessionCommand::SendTurn {
            text, attachments, ..
        } => {
            if turn_id.is_some() {
                // ACP has no steering: one `session/prompt` per turn. The app
                // queues turns, so this should not happen.
                let _ = events
                    .send(AgentEvent::Warning(
                        "a turn is already running; ACP agents cannot take a second prompt mid-turn"
                            .into(),
                    ))
                    .await;
                return;
            }
            *turn_seq += 1;
            let id = format!("turn-{turn_seq}");
            *turn_id = Some(id.clone());
            state.borrow_mut().turn = Some(id.clone());
            let _ = events
                .send(AgentEvent::TurnStarted {
                    turn_id: id.clone(),
                })
                .await;

            let request = acp::PromptRequest::new(
                session.session_id.clone(),
                prompt_blocks(&text, &attachments),
            );
            let connection = connection.clone();
            let turn_tx = turn_tx.clone();
            executor
                .spawn(async move {
                    let result = connection.prompt(request).await;
                    let _ = turn_tx
                        .send(TurnOutcome {
                            turn_id: id,
                            result,
                        })
                        .await;
                })
                .detach();
        }
        SessionCommand::Interrupt => {
            if turn_id.is_none() {
                return;
            }
            // Every in-flight permission request must be answered with
            // `cancelled` first: the protocol requires it, and the agent will
            // not settle the turn otherwise.
            let pending: Vec<Sender<acp::RequestPermissionOutcome>> = state
                .borrow_mut()
                .approvals
                .drain()
                .map(|(_, responder)| responder)
                .collect();
            for responder in pending {
                let _ = responder
                    .send(acp::RequestPermissionOutcome::Cancelled)
                    .await;
            }
            let _ = connection
                .cancel(acp::CancelNotification::new(session.session_id.clone()))
                .await;
            // The agent still owes us the `session/prompt` response carrying
            // `stopReason: cancelled`; the turn completes when it lands.
        }
        SessionCommand::RespondApproval {
            request_id,
            decision,
        } => {
            let responder = state.borrow_mut().approvals.remove(&request_id);
            let Some(responder) = responder else {
                log::warn!("acp: no pending approval {request_id}");
                return;
            };
            let options = state
                .borrow()
                .approval_options
                .get(&request_id)
                .cloned()
                .unwrap_or_default();
            let outcome = match approval_outcome(&decision, &options) {
                Some(outcome) => outcome,
                None => {
                    let _ = events
                        .send(AgentEvent::Warning(
                            "the agent offered no matching permission option; cancelling instead"
                                .into(),
                        ))
                        .await;
                    acp::RequestPermissionOutcome::Cancelled
                }
            };
            let _ = responder.send(outcome).await;
            state.borrow_mut().approval_options.remove(&request_id);
            let _ = events
                .send(AgentEvent::ApprovalResolved {
                    request_id,
                    decision,
                })
                .await;
        }
        SessionCommand::SetOption { id, value } => {
            let Some(origin) = state.borrow().options.origin(&id) else {
                log::warn!("acp: unknown option id `{id}`");
                return;
            };
            match set_option(connection, session, &origin, &value).await {
                Ok(config_options) => {
                    {
                        let mut state = state.borrow_mut();
                        if let Some(options) = config_options.as_deref() {
                            state.options.ingest(None, None, Some(options));
                        }
                        state.options.select(&id, value);
                    }
                    emit_provider_options(state, events).await;
                }
                Err(err) => {
                    let _ = events
                        .send(AgentEvent::Warning(format!(
                            "could not apply `{id}`: {}",
                            describe(&err)
                        )))
                        .await;
                }
            }
        }
        SessionCommand::RespondUserInput { .. } => {
            // ACP has no structured-question equivalent (`session/elicitation`
            // is not in the pinned schema), so we never raise one.
        }
        SessionCommand::SetApprovalMode(_) => {
            let _ = events
                .send(AgentEvent::Warning(
                    "ACP agents own their permission policy; use the agent's own mode selector"
                        .into(),
                ))
                .await;
        }
        SessionCommand::SetInteractionMode(_) => {
            // Build/Plan is a session *mode* in ACP; the traits picker drives it
            // through `SetOption` (`acp:mode`).
        }
        SessionCommand::Shutdown => unreachable!("handled by the caller"),
    }
}

async fn set_option(
    connection: &Rc<acp::ClientSideConnection>,
    session: &Session,
    origin: &OptionOrigin,
    value: &Value,
) -> Result<Option<Vec<acp::SessionConfigOption>>, acp::Error> {
    match origin {
        OptionOrigin::Mode => {
            let Some(mode) = value.as_str() else {
                return Err(acp::Error::invalid_params());
            };
            connection
                .set_session_mode(acp::SetSessionModeRequest::new(
                    session.session_id.clone(),
                    acp::SessionModeId::new(mode),
                ))
                .await?;
            Ok(None)
        }
        OptionOrigin::Model => {
            let Some(model) = value.as_str() else {
                return Err(acp::Error::invalid_params());
            };
            connection
                .set_session_model(acp::SetSessionModelRequest::new(
                    session.session_id.clone(),
                    acp::ModelId::new(model),
                ))
                .await?;
            Ok(None)
        }
        OptionOrigin::Config(config_id) => {
            let value = match value {
                Value::Bool(value) => acp::SessionConfigOptionValue::boolean(*value),
                Value::String(value) => acp::SessionConfigOptionValue::value_id(
                    acp::SessionConfigValueId::new(value.as_str()),
                ),
                _ => return Err(acp::Error::invalid_params()),
            };
            let response = connection
                .set_session_config_option(acp::SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    config_id.clone(),
                    value,
                ))
                .await?;
            Ok(Some(response.config_options))
        }
    }
}

/// Map our four fixed decisions onto the agent's own permission options (so the
/// existing approval UI keeps working), and pass `Option(id)` straight through.
fn approval_outcome(
    decision: &ApprovalDecision,
    options: &[ApprovalOption],
) -> Option<acp::RequestPermissionOutcome> {
    let pick = |kinds: &[ApprovalOptionKind]| -> Option<String> {
        kinds.iter().find_map(|wanted| {
            options
                .iter()
                .find(|option| option.kind == *wanted)
                .map(|option| option.id.clone())
        })
    };
    let selected = match decision {
        ApprovalDecision::Cancel => return Some(acp::RequestPermissionOutcome::Cancelled),
        ApprovalDecision::Option(id) => Some(id.clone()),
        ApprovalDecision::Approve => pick(&[
            ApprovalOptionKind::AllowOnce,
            ApprovalOptionKind::AllowAlways,
        ]),
        ApprovalDecision::ApproveForSession => pick(&[
            ApprovalOptionKind::AllowAlways,
            ApprovalOptionKind::AllowOnce,
        ]),
        ApprovalDecision::Deny => pick(&[
            ApprovalOptionKind::RejectOnce,
            ApprovalOptionKind::RejectAlways,
        ]),
    }?;
    Some(acp::RequestPermissionOutcome::Selected(
        acp::SelectedPermissionOutcome::new(acp::PermissionOptionId::new(selected)),
    ))
}

async fn finish_turn(
    state: &Rc<RefCell<State>>,
    events: &Sender<AgentEvent>,
    turn_id: &str,
    outcome: TurnOutcome,
) {
    // Flush whatever text was still streaming.
    let tail = state.borrow_mut().flush_text();
    for event in tail {
        let _ = events.send(event).await;
    }
    state.borrow_mut().turn = None;

    let (status, message, usage) = match outcome.result {
        Ok(response) => {
            let usage = response.usage.as_ref().map(|usage| TokenUsage {
                input_tokens: Some(usage.input_tokens),
                cached_input_tokens: usage.cached_read_tokens,
                output_tokens: Some(usage.output_tokens),
                used_tokens: None,
                context_window: None,
                total_processed_tokens: Some(usage.total_tokens),
            });
            let (status, message) = stop_reason_status(response.stop_reason);
            (status, message, usage)
        }
        // `-32800 request_cancelled`: the agent aborted the prompt outright
        // instead of returning `stopReason: cancelled`.
        Err(err) if i32::from(err.code) == -32800 => (TurnStatus::Interrupted, None, None),
        Err(err) => (TurnStatus::Failed, Some(describe(&err)), None),
    };
    if let Some(message) = message {
        let _ = events
            .send(AgentEvent::Error {
                message,
                fatal: false,
            })
            .await;
    }
    // Fall back to the live context-window figure from `usage_update`.
    let usage = usage.or_else(|| state.borrow().usage);
    let _ = events
        .send(AgentEvent::TurnCompleted {
            turn_id: turn_id.to_string(),
            status,
            usage,
        })
        .await;
}

/// `stopReason` → canonical turn status, plus the message to surface (if any).
fn stop_reason_status(reason: acp::StopReason) -> (TurnStatus, Option<String>) {
    match reason {
        acp::StopReason::EndTurn => (TurnStatus::Completed, None),
        acp::StopReason::Cancelled => (TurnStatus::Interrupted, None),
        acp::StopReason::Refusal => (
            TurnStatus::Failed,
            Some("The agent refused to continue this turn.".into()),
        ),
        acp::StopReason::MaxTokens => (
            TurnStatus::Failed,
            Some("The agent stopped: token limit reached.".into()),
        ),
        acp::StopReason::MaxTurnRequests => (
            TurnStatus::Failed,
            Some("The agent stopped: too many model requests in one turn.".into()),
        ),
        other => (
            TurnStatus::Failed,
            Some(format!("The agent stopped: {other:?}")),
        ),
    }
}

fn prompt_blocks(text: &str, attachments: &[Attachment]) -> Vec<acp::ContentBlock> {
    let mut blocks = Vec::with_capacity(1 + attachments.len());
    blocks.push(acp::ContentBlock::Text(acp::TextContent::new(
        text.to_string(),
    )));
    for attachment in attachments {
        blocks.push(acp::ContentBlock::Image(acp::ImageContent::new(
            attachment.data_base64.clone(),
            attachment.media_type.clone(),
        )));
    }
    blocks
}

async fn emit_provider_options(state: &Rc<RefCell<State>>, events: &Sender<AgentEvent>) {
    let (descriptors, selections) = {
        let state = state.borrow();
        (state.options.descriptors(), state.options.selections())
    };
    if descriptors.is_empty() {
        return;
    }
    let _ = events
        .send(AgentEvent::ProviderOptions {
            descriptors,
            selections,
        })
        .await;
}

// ---------------------------------------------------------------------------
// Session state + the `session/update` mapping
// ---------------------------------------------------------------------------

/// Where a canonical option id came from, so `SetOption` routes to the right
/// ACP method.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OptionOrigin {
    Mode,
    Model,
    Config(acp::SessionConfigId),
}

/// The agent's self-described modes / models / config options, mapped onto our
/// [`OptionDescriptor`]s (which the composer's traits picker already renders).
#[derive(Default)]
pub(crate) struct OptionRegistry {
    descriptors: Vec<OptionDescriptor>,
    selections: Vec<OptionSelection>,
    origins: HashMap<String, OptionOrigin>,
}

impl OptionRegistry {
    fn ingest(
        &mut self,
        modes: Option<&acp::SessionModeState>,
        models: Option<&acp::SessionModelState>,
        config: Option<&[acp::SessionConfigOption]>,
    ) {
        if let Some(modes) = modes {
            self.upsert(
                OptionDescriptor::Select {
                    id: MODE_OPTION_ID.to_string(),
                    label: "Mode".to_string(),
                    options: modes
                        .available_modes
                        .iter()
                        .map(|mode| SelectOption {
                            value: mode.id.0.to_string(),
                            label: mode.name.clone(),
                            description: mode.description.clone(),
                        })
                        .collect(),
                    default_value: Some(modes.current_mode_id.0.to_string()),
                },
                OptionOrigin::Mode,
                Value::String(modes.current_mode_id.0.to_string()),
            );
        }
        if let Some(models) = models {
            self.upsert(
                OptionDescriptor::Select {
                    id: MODEL_OPTION_ID.to_string(),
                    label: "Model".to_string(),
                    options: models
                        .available_models
                        .iter()
                        .map(|model| SelectOption {
                            value: model.model_id.0.to_string(),
                            label: model.name.clone(),
                            description: model.description.clone(),
                        })
                        .collect(),
                    default_value: Some(models.current_model_id.0.to_string()),
                },
                OptionOrigin::Model,
                Value::String(models.current_model_id.0.to_string()),
            );
        }
        for option in config.unwrap_or_default() {
            let id = format!("{CONFIG_OPTION_PREFIX}{}", option.id.0);
            match &option.kind {
                acp::SessionConfigKind::Select(select) => {
                    let options = match &select.options {
                        acp::SessionConfigSelectOptions::Ungrouped(flat) => {
                            flat.iter().map(select_option).collect()
                        }
                        acp::SessionConfigSelectOptions::Grouped(groups) => groups
                            .iter()
                            .flat_map(|group| {
                                group.options.iter().map(move |option| SelectOption {
                                    value: option.value.0.to_string(),
                                    label: format!("{} · {}", group.name, option.name),
                                    description: option.description.clone(),
                                })
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    self.upsert(
                        OptionDescriptor::Select {
                            id,
                            label: option.name.clone(),
                            options,
                            default_value: Some(select.current_value.0.to_string()),
                        },
                        OptionOrigin::Config(option.id.clone()),
                        Value::String(select.current_value.0.to_string()),
                    );
                }
                acp::SessionConfigKind::Boolean(boolean) => self.upsert(
                    OptionDescriptor::Boolean {
                        id,
                        label: option.name.clone(),
                        default_value: boolean.current_value,
                    },
                    OptionOrigin::Config(option.id.clone()),
                    Value::Bool(boolean.current_value),
                ),
                _ => log::warn!("acp: unsupported config-option kind for `{}`", option.id.0),
            }
        }
    }

    fn upsert(&mut self, descriptor: OptionDescriptor, origin: OptionOrigin, value: Value) {
        let id = descriptor_id(&descriptor).to_string();
        match self
            .descriptors
            .iter_mut()
            .find(|existing| descriptor_id(existing) == id)
        {
            Some(existing) => *existing = descriptor,
            None => self.descriptors.push(descriptor),
        }
        self.origins.insert(id.clone(), origin);
        self.select(&id, value);
    }

    fn select(&mut self, id: &str, value: Value) {
        match self.selections.iter_mut().find(|s| s.id == id) {
            Some(selection) => selection.value = value,
            None => self.selections.push(OptionSelection {
                id: id.to_string(),
                value,
            }),
        }
    }

    fn origin(&self, id: &str) -> Option<OptionOrigin> {
        self.origins.get(id).cloned()
    }

    fn descriptors(&self) -> Vec<OptionDescriptor> {
        self.descriptors.clone()
    }

    fn selections(&self) -> Vec<OptionSelection> {
        self.selections.clone()
    }
}

fn select_option(option: &acp::SessionConfigSelectOption) -> SelectOption {
    SelectOption {
        value: option.value.0.to_string(),
        label: option.name.clone(),
        description: option.description.clone(),
    }
}

fn descriptor_id(descriptor: &OptionDescriptor) -> &str {
    match descriptor {
        OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => id,
    }
}

/// A tool call as last known. `tool_call_update` is a partial patch, so the
/// merged state lives here and every update re-renders the whole item.
#[derive(Debug, Clone, Default)]
struct ToolState {
    title: String,
    kind: acp::ToolKind,
    status: acp::ToolCallStatus,
    content: Vec<acp::ToolCallContent>,
    locations: Vec<acp::ToolCallLocation>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    announced: bool,
}

/// The text block currently streaming (assistant prose or thinking).
struct TextStream {
    id: String,
    kind: DeltaKind,
    text: String,
}

pub(crate) struct State {
    cwd: PathBuf,
    session_id: Option<acp::SessionId>,
    turn: Option<String>,
    /// True while `session/load` replays history we already have on disk.
    replaying: bool,
    tools: HashMap<String, ToolState>,
    text: Option<TextStream>,
    approvals: HashMap<String, Sender<acp::RequestPermissionOutcome>>,
    approval_options: HashMap<String, Vec<ApprovalOption>>,
    approval_seq: u64,
    text_seq: u64,
    terminal_seq: u64,
    terminals: HashMap<String, Rc<Terminal>>,
    usage: Option<TokenUsage>,
    options: OptionRegistry,
}

impl State {
    fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            session_id: None,
            turn: None,
            replaying: false,
            tools: HashMap::new(),
            text: None,
            approvals: HashMap::new(),
            approval_options: HashMap::new(),
            approval_seq: 0,
            text_seq: 0,
            terminal_seq: 0,
            terminals: HashMap::new(),
            usage: None,
            options: OptionRegistry::default(),
        }
    }

    /// Close the open text block, emitting its final `ItemCompleted`.
    fn flush_text(&mut self) -> Vec<AgentEvent> {
        let Some(stream) = self.text.take() else {
            return Vec::new();
        };
        if stream.text.is_empty() {
            return Vec::new();
        }
        let content = match stream.kind {
            DeltaKind::ReasoningText => ItemContent::Reasoning { text: stream.text },
            _ => ItemContent::AssistantMessage { text: stream.text },
        };
        vec![AgentEvent::ItemCompleted(ThreadItem {
            id: stream.id,
            content,
        })]
    }

    /// Append a streaming chunk, opening a new item when the block changes.
    fn push_text(&mut self, kind: DeltaKind, text: String) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let same_block = self.text.as_ref().is_some_and(|stream| stream.kind == kind);
        if !same_block {
            events.extend(self.flush_text());
            self.text_seq += 1;
            let prefix = match kind {
                DeltaKind::ReasoningText => "thought",
                _ => "msg",
            };
            self.text = Some(TextStream {
                id: format!("{prefix}-{}", self.text_seq),
                kind,
                text: String::new(),
            });
        }
        let stream = self.text.as_mut().expect("stream just opened");
        stream.text.push_str(&text);
        events.push(AgentEvent::Delta {
            item_id: stream.id.clone(),
            kind,
            text,
        });
        events
    }

    /// Map one `session/update` onto canonical events, merging our tool-call and
    /// option state along the way. Pure w.r.t. the outside world — this is the
    /// function the mapping tests drive.
    pub(crate) fn apply_update(&mut self, update: acp::SessionUpdate) -> Vec<AgentEvent> {
        match update {
            // The app synthesizes the canonical user message at send time;
            // rendering the agent's echo of it would double it.
            acp::SessionUpdate::UserMessageChunk(_) => Vec::new(),
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                let text = content_text(&chunk.content);
                if text.is_empty() {
                    return Vec::new();
                }
                self.push_text(DeltaKind::AssistantText, text)
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = content_text(&chunk.content);
                if text.is_empty() {
                    return Vec::new();
                }
                self.push_text(DeltaKind::ReasoningText, text)
            }
            acp::SessionUpdate::ToolCall(call) => {
                let mut events = self.flush_text();
                let id = call.tool_call_id.0.to_string();
                let tool = ToolState {
                    title: call.title,
                    kind: call.kind,
                    status: call.status,
                    content: call.content,
                    locations: call.locations,
                    raw_input: call.raw_input,
                    raw_output: call.raw_output,
                    announced: true,
                };
                let item = self.tool_item(&id, &tool);
                let existed = self
                    .tools
                    .insert(id, tool)
                    .is_some_and(|previous| previous.announced);
                events.push(if existed {
                    AgentEvent::ItemUpdated(item)
                } else {
                    AgentEvent::ItemStarted(item)
                });
                events
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                let mut events = self.flush_text();
                let id = update.tool_call_id.0.to_string();
                let entry = self.tools.entry(id.clone()).or_default();
                let fields = update.fields;
                if let Some(title) = fields.title {
                    entry.title = title;
                }
                if let Some(kind) = fields.kind {
                    entry.kind = kind;
                }
                if let Some(status) = fields.status {
                    entry.status = status;
                }
                // `content` and `locations` are whole-array replacements.
                if let Some(content) = fields.content {
                    entry.content = content;
                }
                if let Some(locations) = fields.locations {
                    entry.locations = locations;
                }
                if let Some(raw_input) = fields.raw_input {
                    entry.raw_input = Some(raw_input);
                }
                if let Some(raw_output) = fields.raw_output {
                    entry.raw_output = Some(raw_output);
                }
                let announced = std::mem::replace(&mut entry.announced, true);
                let status = entry.status;
                let tool = entry.clone();
                let item = self.tool_item(&id, &tool);
                events.push(match (announced, status) {
                    (false, _) => AgentEvent::ItemStarted(item),
                    (true, acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed) => {
                        AgentEvent::ItemCompleted(item)
                    }
                    _ => AgentEvent::ItemUpdated(item),
                });
                events
            }
            acp::SessionUpdate::Plan(plan) => vec![AgentEvent::PlanUpdated {
                turn_id: self.turn.clone(),
                explanation: None,
                steps: plan.entries.iter().map(plan_step).collect(),
            }],
            acp::SessionUpdate::AvailableCommandsUpdate(update) => {
                vec![AgentEvent::ProviderCommands {
                    commands: update
                        .available_commands
                        .iter()
                        .map(|command| ProviderCommand {
                            name: command.name.clone(),
                            description: Some(command.description.clone()),
                            kind: ProviderCommandKind::Command,
                        })
                        .collect(),
                }]
            }
            acp::SessionUpdate::CurrentModeUpdate(update) => {
                self.options.select(
                    MODE_OPTION_ID,
                    Value::String(update.current_mode_id.0.to_string()),
                );
                vec![AgentEvent::ProviderOptions {
                    descriptors: self.options.descriptors(),
                    selections: self.options.selections(),
                }]
            }
            acp::SessionUpdate::ConfigOptionUpdate(update) => {
                self.options
                    .ingest(None, None, Some(&update.config_options));
                vec![AgentEvent::ProviderOptions {
                    descriptors: self.options.descriptors(),
                    selections: self.options.selections(),
                }]
            }
            acp::SessionUpdate::UsageUpdate(usage) => {
                let usage = TokenUsage {
                    used_tokens: Some(usage.used),
                    context_window: Some(usage.size),
                    ..Default::default()
                };
                self.usage = Some(usage);
                vec![AgentEvent::TokenUsage(usage)]
            }
            // Session titles are ours (the sidebar names sessions from the first
            // user message), and unknown variants degrade to nothing.
            _ => Vec::new(),
        }
    }

    /// The canonical item for a tool call, keyed by [`acp::ToolKind`].
    fn tool_item(&self, id: &str, tool: &ToolState) -> ThreadItem {
        let status = map_status(tool.status);
        let content = match tool.kind {
            acp::ToolKind::Execute => ItemContent::CommandExecution {
                command: command_of(tool),
                output: self.tool_output(tool),
                exit_code: self.exit_code_of(tool),
                status,
            },
            acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move => {
                let changes = file_changes(tool);
                if changes.is_empty() {
                    tool_call_content(tool, status, self.tool_output(tool))
                } else {
                    ItemContent::FileChange { changes, status }
                }
            }
            acp::ToolKind::Think => {
                let text = self.tool_output(tool);
                ItemContent::Reasoning {
                    text: if text.is_empty() {
                        tool.title.clone()
                    } else {
                        text
                    },
                }
            }
            // read | search | fetch | switch_mode | other
            _ => tool_call_content(tool, status, self.tool_output(tool)),
        };
        ThreadItem {
            id: id.to_string(),
            content,
        }
    }

    /// Everything the tool produced, as display text: its content blocks plus
    /// the live output of any terminal it embedded.
    fn tool_output(&self, tool: &ToolState) -> String {
        let mut parts: Vec<String> = Vec::new();
        for content in &tool.content {
            match content {
                acp::ToolCallContent::Content(block) => {
                    let text = content_text(&block.content);
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
                acp::ToolCallContent::Terminal(terminal) => {
                    if let Some(terminal) = self.terminals.get(terminal.terminal_id.0.as_ref()) {
                        let output = terminal.output.borrow();
                        if !output.is_empty() {
                            parts.push(output.clone());
                        }
                    }
                }
                // Diffs render as FileChange, not as text.
                _ => {}
            }
        }
        if parts.is_empty()
            && let Some(raw) = &tool.raw_output
        {
            match raw.as_str() {
                Some(text) => parts.push(text.to_string()),
                None if !raw.is_null() => parts.push(raw.to_string()),
                None => {}
            }
        }
        parts.join("\n")
    }

    fn exit_code_of(&self, tool: &ToolState) -> Option<i32> {
        for content in &tool.content {
            if let acp::ToolCallContent::Terminal(terminal) = content
                && let Some(terminal) = self.terminals.get(terminal.terminal_id.0.as_ref())
                && let Some(status) = terminal.exit.borrow().as_ref()
            {
                return status.exit_code.map(|code| code as i32);
            }
        }
        tool.raw_output
            .as_ref()
            .and_then(|raw| raw.get("exitCode").or_else(|| raw.get("exit_code")))
            .and_then(Value::as_i64)
            .map(|code| code as i32)
    }

    /// Reject any path the agent asks for that escapes the session's cwd.
    fn resolve_path(&self, path: &Path) -> Result<PathBuf, acp::Error> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        let normalized = normalize(&absolute);
        if !normalized.starts_with(normalize(&self.cwd)) {
            return Err(acp::Error::new(
                -32602,
                format!(
                    "path `{}` is outside the session working directory",
                    path.display()
                ),
            ));
        }
        Ok(normalized)
    }
}

/// Lexical path normalization (the file may not exist yet, so `canonicalize` is
/// not an option for writes).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The shell command behind an `execute` tool call: the underlying tool's own
/// arguments when it exposed them, else the agent's title.
fn command_of(tool: &ToolState) -> String {
    tool.raw_input
        .as_ref()
        .and_then(|input| input.get("command").or_else(|| input.get("cmd")))
        .and_then(|command| match command {
            Value::String(command) => Some(command.clone()),
            Value::Array(parts) => Some(
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
        .unwrap_or_else(|| tool.title.clone())
}

fn tool_call_content(tool: &ToolState, status: ItemStatus, output: String) -> ItemContent {
    ItemContent::ToolCall {
        name: tool.title.clone(),
        input: tool.raw_input.clone().unwrap_or(Value::Null),
        output: (!output.is_empty()).then_some(output),
        status,
    }
}

/// `diff` content blocks → canonical [`FileChange`]s (with a unified diff, so
/// the diff panel renders them like every other provider's edits).
fn file_changes(tool: &ToolState) -> Vec<FileChange> {
    let mut changes: Vec<FileChange> = tool
        .content
        .iter()
        .filter_map(|content| match content {
            acp::ToolCallContent::Diff(diff) => {
                let path = diff.path.to_string_lossy().into_owned();
                Some(FileChange {
                    kind: match (tool.kind, diff.old_text.as_deref()) {
                        (acp::ToolKind::Delete, _) => FileChangeKind::Delete,
                        (acp::ToolKind::Move, _) => FileChangeKind::Rename,
                        (_, None | Some("")) => FileChangeKind::Create,
                        _ => FileChangeKind::Modify,
                    },
                    diff: Some(unified_diff(
                        &path,
                        diff.old_text.as_deref().unwrap_or(""),
                        &diff.new_text,
                    )),
                    path,
                })
            }
            _ => None,
        })
        .collect();
    if changes.is_empty() && matches!(tool.kind, acp::ToolKind::Delete | acp::ToolKind::Move) {
        // Deletes and renames carry no diff; the locations are all we get.
        changes = tool
            .locations
            .iter()
            .map(|location| FileChange {
                path: location.path.to_string_lossy().into_owned(),
                kind: match tool.kind {
                    acp::ToolKind::Delete => FileChangeKind::Delete,
                    _ => FileChangeKind::Rename,
                },
                diff: None,
            })
            .collect();
    }
    changes
}

/// A whole-file unified diff. ACP hands us before/after text rather than a
/// patch, and the diff panel wants `@@` hunks; a single hunk covering the file
/// is exactly what the panel's full-file path already renders.
fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut out = String::new();
    if old_lines.is_empty() {
        out.push_str("--- /dev/null\n");
    } else {
        out.push_str(&format!("--- a/{path}\n"));
    }
    if new_lines.is_empty() {
        out.push_str("+++ /dev/null\n");
    } else {
        out.push_str(&format!("+++ b/{path}\n"));
    }
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        usize::from(!old_lines.is_empty()),
        old_lines.len(),
        usize::from(!new_lines.is_empty()),
        new_lines.len()
    ));
    // Trim the common prefix/suffix so the (very common) single-line edit does
    // not render as a whole-file rewrite.
    let prefix = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(old, new)| old == new)
        .count();
    let suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take_while(|(old, new)| old == new)
        .count()
        .min(old_lines.len() - prefix)
        .min(new_lines.len() - prefix);
    for line in &old_lines[..prefix] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in &old_lines[prefix..old_lines.len() - suffix] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &new_lines[prefix..new_lines.len() - suffix] {
        out.push_str(&format!("+{line}\n"));
    }
    for line in &old_lines[old_lines.len() - suffix..] {
        out.push_str(&format!(" {line}\n"));
    }
    out
}

fn map_status(status: acp::ToolCallStatus) -> ItemStatus {
    match status {
        acp::ToolCallStatus::Pending | acp::ToolCallStatus::InProgress => ItemStatus::InProgress,
        acp::ToolCallStatus::Completed => ItemStatus::Completed,
        acp::ToolCallStatus::Failed => ItemStatus::Failed,
        _ => ItemStatus::InProgress,
    }
}

fn plan_step(entry: &acp::PlanEntry) -> PlanStep {
    PlanStep {
        step: entry.content.clone(),
        status: match entry.status {
            acp::PlanEntryStatus::InProgress => PlanStepStatus::InProgress,
            acp::PlanEntryStatus::Completed => PlanStepStatus::Completed,
            _ => PlanStepStatus::Pending,
        },
    }
}

/// Displayable text for a content block (images/audio degrade to a marker).
fn content_text(block: &acp::ContentBlock) -> String {
    match block {
        acp::ContentBlock::Text(text) => text.text.clone(),
        acp::ContentBlock::ResourceLink(link) => link.uri.clone(),
        acp::ContentBlock::Resource(resource) => match &resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(text) => text.text.clone(),
            _ => String::new(),
        },
        acp::ContentBlock::Image(_) => "[image]".to_string(),
        acp::ContentBlock::Audio(_) => "[audio]".to_string(),
        _ => String::new(),
    }
}

/// The approval an agent's `session/request_permission` becomes.
pub(crate) fn approval_request(
    id: String,
    turn_id: Option<String>,
    tool: &acp::ToolCallUpdate,
    options: &[acp::PermissionOption],
) -> ApprovalRequest {
    let fields = &tool.fields;
    let title = fields.title.clone().unwrap_or_default();
    let kind = fields.kind.unwrap_or_default();
    let raw_input = fields.raw_input.clone().unwrap_or(Value::Null);
    let approval_kind = match kind {
        acp::ToolKind::Execute => ApprovalKind::ExecCommand {
            command: command_of(&ToolState {
                title: title.clone(),
                raw_input: fields.raw_input.clone(),
                ..Default::default()
            }),
            cwd: None,
            reason: None,
        },
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move => {
            let changes = file_changes(&ToolState {
                title: title.clone(),
                kind,
                content: fields.content.clone().unwrap_or_default(),
                locations: fields.locations.clone().unwrap_or_default(),
                raw_input: fields.raw_input.clone(),
                ..Default::default()
            });
            if changes.is_empty() {
                ApprovalKind::ToolUse {
                    name: title.clone(),
                    input: raw_input,
                    detail: title,
                }
            } else {
                ApprovalKind::FileChange {
                    changes,
                    reason: None,
                }
            }
        }
        acp::ToolKind::Read | acp::ToolKind::Search | acp::ToolKind::Fetch => {
            ApprovalKind::FileRead { detail: title }
        }
        _ => ApprovalKind::ToolUse {
            name: title.clone(),
            input: raw_input,
            detail: title,
        },
    };
    ApprovalRequest {
        id,
        turn_id,
        kind: approval_kind,
        options: options.iter().map(map_permission_option).collect(),
    }
}

fn map_permission_option(option: &acp::PermissionOption) -> ApprovalOption {
    ApprovalOption {
        id: option.option_id.0.to_string(),
        label: option.name.clone(),
        kind: match option.kind {
            acp::PermissionOptionKind::AllowOnce => ApprovalOptionKind::AllowOnce,
            acp::PermissionOptionKind::AllowAlways => ApprovalOptionKind::AllowAlways,
            acp::PermissionOptionKind::RejectAlways => ApprovalOptionKind::RejectAlways,
            _ => ApprovalOptionKind::RejectOnce,
        },
    }
}

// ---------------------------------------------------------------------------
// The `Client` half of the connection (agent → us)
// ---------------------------------------------------------------------------

struct AcpClient {
    events: Sender<AgentEvent>,
    state: Rc<RefCell<State>>,
    cwd: PathBuf,
    executor: Rc<smol::LocalExecutor<'static>>,
}

#[async_trait::async_trait(?Send)]
impl Client for AcpClient {
    async fn session_notification(&self, args: acp::SessionNotification) -> Result<(), acp::Error> {
        // While `session/load` replays the conversation we already have on disk,
        // swallow everything: our JSONL log is the source of truth and the
        // timeline was folded from it before the process even started.
        if self.state.borrow().replaying {
            return Ok(());
        }
        let events = self.state.borrow_mut().apply_update(args.update);
        for event in events {
            let _ = self.events.send(event).await;
        }
        Ok(())
    }

    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        let (request_id, turn) = {
            let mut state = self.state.borrow_mut();
            state.approval_seq += 1;
            (
                format!("acp-approval-{}", state.approval_seq),
                state.turn.clone(),
            )
        };
        let request = approval_request(request_id.clone(), turn, &args.tool_call, &args.options);
        let (responder, decided) = async_channel::bounded(1);
        {
            let mut state = self.state.borrow_mut();
            state.approvals.insert(request_id.clone(), responder);
            state
                .approval_options
                .insert(request_id.clone(), request.options.clone());
            // Keep the tool card in step with what we are asking about.
            let id = args.tool_call.tool_call_id.0.to_string();
            let entry = state.tools.entry(id).or_default();
            if let Some(title) = args.tool_call.fields.title.clone() {
                entry.title = title;
            }
            if let Some(kind) = args.tool_call.fields.kind {
                entry.kind = kind;
            }
            if let Some(raw_input) = args.tool_call.fields.raw_input.clone() {
                entry.raw_input = Some(raw_input);
            }
        }
        let _ = self
            .events
            .send(AgentEvent::ApprovalRequested(request))
            .await;

        let outcome = decided
            .recv()
            .await
            .unwrap_or(acp::RequestPermissionOutcome::Cancelled);
        let mut state = self.state.borrow_mut();
        state.approvals.remove(&request_id);
        state.approval_options.remove(&request_id);
        drop(state);
        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        let path = self.state.borrow().resolve_path(&args.path)?;
        let content = smol::fs::read_to_string(&path).await.map_err(|err| {
            acp::Error::new(-32603, format!("could not read {}: {err}", path.display()))
        })?;
        // `line` is 1-based; `limit` counts lines from there.
        let content = match (args.line, args.limit) {
            (None, None) => content,
            (line, limit) => {
                let start = line.unwrap_or(1).saturating_sub(1) as usize;
                let lines: Vec<&str> = content.lines().skip(start).collect();
                let end = limit.map_or(lines.len(), |limit| lines.len().min(limit as usize));
                lines[..end].join("\n")
            }
        };
        Ok(acp::ReadTextFileResponse::new(content))
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse, acp::Error> {
        let path = self.state.borrow().resolve_path(&args.path)?;
        if let Some(parent) = path.parent() {
            smol::fs::create_dir_all(parent).await.map_err(|err| {
                acp::Error::new(
                    -32603,
                    format!("could not create {}: {err}", parent.display()),
                )
            })?;
        }
        smol::fs::write(&path, args.content).await.map_err(|err| {
            acp::Error::new(-32603, format!("could not write {}: {err}", path.display()))
        })?;
        Ok(acp::WriteTextFileResponse::new())
    }

    async fn create_terminal(
        &self,
        args: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalResponse, acp::Error> {
        let cwd = match &args.cwd {
            Some(cwd) => self.state.borrow().resolve_path(cwd)?,
            None => self.cwd.clone(),
        };
        let terminal = Terminal::spawn(
            &self.executor,
            &args.command,
            &args.args,
            &args.env,
            &cwd,
            args.output_byte_limit
                .unwrap_or(DEFAULT_TERMINAL_OUTPUT_LIMIT),
        )?;
        let id = {
            let mut state = self.state.borrow_mut();
            state.terminal_seq += 1;
            let id = format!("term-{}", state.terminal_seq);
            state.terminals.insert(id.clone(), terminal);
            id
        };
        Ok(acp::CreateTerminalResponse::new(acp::TerminalId::new(id)))
    }

    async fn terminal_output(
        &self,
        args: acp::TerminalOutputRequest,
    ) -> Result<acp::TerminalOutputResponse, acp::Error> {
        let terminal = self.terminal(&args.terminal_id)?;
        let output = terminal.output.borrow().clone();
        let truncated = *terminal.truncated.borrow();
        let exit_status = terminal.exit.borrow().clone();
        Ok(acp::TerminalOutputResponse::new(output, truncated).exit_status(exit_status))
    }

    async fn wait_for_terminal_exit(
        &self,
        args: acp::WaitForTerminalExitRequest,
    ) -> Result<acp::WaitForTerminalExitResponse, acp::Error> {
        let terminal = self.terminal(&args.terminal_id)?;
        // The sender is dropped once the process exits, closing the channel.
        let _ = terminal.done.recv().await;
        let exit_status = terminal
            .exit
            .borrow()
            .clone()
            .unwrap_or_else(acp::TerminalExitStatus::new);
        Ok(acp::WaitForTerminalExitResponse::new(exit_status))
    }

    async fn kill_terminal(
        &self,
        args: acp::KillTerminalRequest,
    ) -> Result<acp::KillTerminalResponse, acp::Error> {
        self.terminal(&args.terminal_id)?.kill();
        Ok(acp::KillTerminalResponse::new())
    }

    async fn release_terminal(
        &self,
        args: acp::ReleaseTerminalRequest,
    ) -> Result<acp::ReleaseTerminalResponse, acp::Error> {
        let terminal = self
            .state
            .borrow_mut()
            .terminals
            .remove(args.terminal_id.0.as_ref());
        if let Some(terminal) = terminal {
            terminal.kill();
        }
        Ok(acp::ReleaseTerminalResponse::new())
    }
}

impl AcpClient {
    fn terminal(&self, id: &acp::TerminalId) -> Result<Rc<Terminal>, acp::Error> {
        self.state
            .borrow()
            .terminals
            .get(id.0.as_ref())
            .cloned()
            .ok_or_else(|| acp::Error::new(-32002, format!("unknown terminal `{}`", id.0)))
    }
}

// ---------------------------------------------------------------------------
// Terminals (client-owned processes the agent drives)
// ---------------------------------------------------------------------------

/// A command the agent asked us to run. Headless: we capture the output and
/// serve `terminal/output` / `terminal/wait_for_exit` from it, and the text is
/// folded into the owning tool card (`ToolCallContent::Terminal`). Wiring these
/// into the terminal drawer needs a canonical event the contract does not have.
struct Terminal {
    child: RefCell<smol::process::Child>,
    output: RefCell<String>,
    truncated: RefCell<bool>,
    exit: RefCell<Option<acp::TerminalExitStatus>>,
    /// Closed (never sent on) once the process has exited.
    done: Receiver<()>,
}

impl Terminal {
    fn spawn(
        executor: &Rc<smol::LocalExecutor<'static>>,
        command: &str,
        args: &[String],
        env: &[acp::EnvVariable],
        cwd: &Path,
        limit: u64,
    ) -> Result<Rc<Self>, acp::Error> {
        let program = crate::resolve_binary(None, command)
            .map_err(|err| acp::Error::new(-32603, err.to_string()))?;
        let mut cmd = crate::process::async_command(&program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for var in env {
            cmd.env(&var.name, &var.value);
        }
        let mut child = cmd
            .spawn()
            .map_err(|err| acp::Error::new(-32603, format!("could not run `{command}`: {err}")))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (done_tx, done) = async_channel::bounded::<()>(1);

        let terminal = Rc::new(Terminal {
            child: RefCell::new(child),
            output: RefCell::new(String::new()),
            truncated: RefCell::new(false),
            exit: RefCell::new(None),
            done,
        });

        // stdout and stderr interleave into one buffer, as they do in a terminal.
        let streams: [Box<dyn futures_lite::AsyncRead + Unpin>; 2] =
            [Box::new(stdout), Box::new(stderr)];
        for stream in streams {
            executor
                .spawn({
                    let terminal = terminal.clone();
                    async move {
                        let mut stream = stream;
                        let mut buf = [0u8; 4096];
                        loop {
                            match stream.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(read) => terminal.append(&buf[..read], limit),
                            }
                        }
                    }
                })
                .detach();
        }

        executor
            .spawn({
                let terminal = terminal.clone();
                async move {
                    loop {
                        let status = terminal.child.borrow_mut().try_status();
                        match status {
                            Ok(Some(status)) => {
                                *terminal.exit.borrow_mut() = Some(exit_status(&status));
                                break;
                            }
                            Err(err) => {
                                log::warn!("acp terminal: {err}");
                                break;
                            }
                            Ok(None) => smol::Timer::after(Duration::from_millis(25)).await,
                        };
                    }
                    // Closing the channel wakes every `wait_for_exit`.
                    drop(done_tx);
                }
            })
            .detach();

        Ok(terminal)
    }

    fn append(&self, bytes: &[u8], limit: u64) {
        let mut output = self.output.borrow_mut();
        output.push_str(&String::from_utf8_lossy(bytes));
        let limit = limit as usize;
        if output.len() > limit {
            // Keep the tail (what ACP asks for), cutting on a char boundary.
            let cut = output.len() - limit;
            let cut = (cut..output.len())
                .find(|index| output.is_char_boundary(*index))
                .unwrap_or(output.len());
            *output = output[cut..].to_string();
            *self.truncated.borrow_mut() = true;
        }
    }

    fn kill(&self) {
        let _ = self.child.borrow_mut().kill();
    }
}

fn exit_status(status: &std::process::ExitStatus) -> acp::TerminalExitStatus {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal().map(|signal| signal.to_string())
    };
    #[cfg(not(unix))]
    let signal: Option<String> = None;
    acp::TerminalExitStatus::new()
        .exit_code(status.code().map(|code| code as u32))
        .signal(signal)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State::new(PathBuf::from("/tmp/tcode-acp-test"))
    }

    fn update(json: Value) -> acp::SessionUpdate {
        serde_json::from_value(json).expect("valid session/update payload")
    }

    #[test]
    fn npx_recipe_becomes_npm_exec() {
        let (program, args) = launch_command(&AcpLaunch::Npx {
            package: "@google/gemini-cli@0.50.0".into(),
            args: vec!["--acp".into()],
            env: Vec::new(),
        })
        .expect("npm must resolve on PATH");
        assert_eq!(program.file_stem().unwrap(), "npm");
        assert_eq!(
            args,
            vec!["exec", "--yes", "--", "@google/gemini-cli@0.50.0", "--acp"]
        );
    }

    #[test]
    fn binary_recipe_runs_as_given() {
        let (program, args) = launch_command(&AcpLaunch::Binary {
            command: PathBuf::from("/opt/acp/goose"),
            args: vec!["acp".into()],
            env: Vec::new(),
        })
        .unwrap();
        assert_eq!(program, PathBuf::from("/opt/acp/goose"));
        assert_eq!(args, vec!["acp".to_string()]);
    }

    /// The preview MCP server is a loopback HTTP endpoint: it may only be handed
    /// to agents that advertise `mcpCapabilities.http`.
    #[test]
    fn mcp_server_is_gated_on_the_http_capability() {
        let registration = McpRegistration {
            url: "http://127.0.0.1:5321/mcp".into(),
            bearer_token: "tok".into(),
        };
        let mut caps = acp::AgentCapabilities::default();
        assert!(
            mcp_servers(Some(&registration), &caps).is_empty(),
            "an agent without mcpCapabilities.http must not be sent the HTTP server"
        );

        caps.mcp_capabilities.http = true;
        let servers = mcp_servers(Some(&registration), &caps);
        let value = serde_json::to_value(&servers[0]).unwrap();
        assert_eq!(value["type"], "http");
        assert_eq!(value["name"], "tcode_preview");
        assert_eq!(value["url"], "http://127.0.0.1:5321/mcp");
        assert_eq!(value["headers"][0]["name"], "Authorization");
        assert_eq!(value["headers"][0]["value"], "Bearer tok");

        // No preview server running → nothing to send, capability or not.
        assert!(mcp_servers(None, &caps).is_empty());
    }

    #[test]
    fn agent_message_chunks_stream_then_complete() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "Hel" }
        })));
        let item_id = match &events[0] {
            AgentEvent::Delta {
                item_id,
                kind,
                text,
            } => {
                assert_eq!(*kind, DeltaKind::AssistantText);
                assert_eq!(text, "Hel");
                item_id.clone()
            }
            other => panic!("expected Delta, got {other:?}"),
        };
        state.apply_update(update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "lo" }
        })));
        match &state.flush_text()[0] {
            AgentEvent::ItemCompleted(item) => {
                assert_eq!(item.id, item_id);
                match &item.content {
                    ItemContent::AssistantMessage { text } => assert_eq!(text, "Hello"),
                    other => panic!("expected AssistantMessage, got {other:?}"),
                }
            }
            other => panic!("expected ItemCompleted, got {other:?}"),
        }
    }

    #[test]
    fn thought_chunks_map_to_reasoning_and_close_the_prose_block() {
        let mut state = state();
        state.apply_update(update(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hi" }
        })));
        let events = state.apply_update(update(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "pondering" }
        })));
        // The open assistant block completes before the thought stream opens.
        assert!(matches!(events[0], AgentEvent::ItemCompleted(_)));
        match &events[1] {
            AgentEvent::Delta { kind, text, .. } => {
                assert_eq!(*kind, DeltaKind::ReasoningText);
                assert_eq!(text, "pondering");
            }
            other => panic!("expected Delta, got {other:?}"),
        }
    }

    #[test]
    fn user_message_chunks_are_ignored() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": "echo of my own prompt" }
        })));
        assert!(events.is_empty(), "user echoes must not be re-rendered");
    }

    #[test]
    fn execute_tool_call_maps_to_a_command_execution() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Run tests",
            "kind": "execute",
            "status": "in_progress",
            "rawInput": { "command": "cargo test" }
        })));
        match &events[0] {
            AgentEvent::ItemStarted(item) => {
                assert_eq!(item.id, "t1");
                match &item.content {
                    ItemContent::CommandExecution {
                        command, status, ..
                    } => {
                        assert_eq!(command, "cargo test");
                        assert_eq!(*status, ItemStatus::InProgress);
                    }
                    other => panic!("expected CommandExecution, got {other:?}"),
                }
            }
            other => panic!("expected ItemStarted, got {other:?}"),
        }

        // A partial patch merges into the merged state and completes the item.
        let events = state.apply_update(update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "ok" } }],
            "rawOutput": { "exitCode": 0 }
        })));
        match &events[0] {
            AgentEvent::ItemCompleted(item) => match &item.content {
                ItemContent::CommandExecution {
                    command,
                    output,
                    exit_code,
                    status,
                } => {
                    assert_eq!(command, "cargo test", "rawInput must survive the patch");
                    assert_eq!(output, "ok");
                    assert_eq!(*exit_code, Some(0));
                    assert_eq!(*status, ItemStatus::Completed);
                }
                other => panic!("expected CommandExecution, got {other:?}"),
            },
            other => panic!("expected ItemCompleted, got {other:?}"),
        }
    }

    #[test]
    fn edit_tool_call_maps_diff_blocks_to_file_changes() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t2",
            "title": "Edit main.rs",
            "kind": "edit",
            "status": "completed",
            "content": [{
                "type": "diff",
                "path": "/repo/main.rs",
                "oldText": "fn main() {}\n",
                "newText": "fn main() { println!(\"hi\"); }\n"
            }]
        })));
        match &events[0] {
            AgentEvent::ItemStarted(item) => match &item.content {
                ItemContent::FileChange { changes, status } => {
                    assert_eq!(changes.len(), 1);
                    assert_eq!(changes[0].path, "/repo/main.rs");
                    assert_eq!(changes[0].kind, FileChangeKind::Modify);
                    let diff = changes[0].diff.as_ref().unwrap();
                    assert!(diff.contains("@@"), "{diff}");
                    assert!(diff.contains("-fn main() {}"), "{diff}");
                    assert!(diff.contains("+fn main() { println!(\"hi\"); }"), "{diff}");
                    assert_eq!(*status, ItemStatus::Completed);
                }
                other => panic!("expected FileChange, got {other:?}"),
            },
            other => panic!("expected ItemStarted, got {other:?}"),
        }
    }

    #[test]
    fn a_new_file_edit_is_a_create() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t3",
            "title": "Create notes.md",
            "kind": "edit",
            "status": "completed",
            "content": [{ "type": "diff", "path": "notes.md", "oldText": null, "newText": "hi\n" }]
        })));
        match &events[0] {
            AgentEvent::ItemStarted(item) => match &item.content {
                ItemContent::FileChange { changes, .. } => {
                    assert_eq!(changes[0].kind, FileChangeKind::Create);
                    assert!(changes[0].diff.as_ref().unwrap().contains("--- /dev/null"));
                }
                other => panic!("expected FileChange, got {other:?}"),
            },
            other => panic!("expected ItemStarted, got {other:?}"),
        }
    }

    #[test]
    fn delete_and_move_fall_back_to_locations() {
        for (kind, expected) in [
            ("delete", FileChangeKind::Delete),
            ("move", FileChangeKind::Rename),
        ] {
            let mut state = state();
            let events = state.apply_update(update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "d1",
                "title": "Remove it",
                "kind": kind,
                "status": "completed",
                "locations": [{ "path": "old.rs" }]
            })));
            match &events[0] {
                AgentEvent::ItemStarted(item) => match &item.content {
                    ItemContent::FileChange { changes, .. } => {
                        assert_eq!(changes[0].path, "old.rs");
                        assert_eq!(changes[0].kind, expected);
                    }
                    other => panic!("{kind}: expected FileChange, got {other:?}"),
                },
                other => panic!("{kind}: expected ItemStarted, got {other:?}"),
            }
        }
    }

    #[test]
    fn read_search_fetch_and_other_map_to_tool_cards_with_raw_payloads() {
        for (kind, id) in [
            ("read", "r1"),
            ("search", "r2"),
            ("fetch", "r3"),
            ("other", "r4"),
            ("switch_mode", "r5"),
        ] {
            let mut state = state();
            let events = state.apply_update(update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": "Read file",
                "kind": kind,
                "status": "completed",
                "rawInput": { "path": "/repo/x.rs" },
                "content": [{ "type": "content", "content": { "type": "text", "text": "body" } }]
            })));
            match &events[0] {
                AgentEvent::ItemStarted(item) => match &item.content {
                    ItemContent::ToolCall {
                        name,
                        input,
                        output,
                        status,
                    } => {
                        assert_eq!(name, "Read file");
                        assert_eq!(input["path"], "/repo/x.rs", "rawInput must ride along");
                        assert_eq!(output.as_deref(), Some("body"));
                        assert_eq!(*status, ItemStatus::Completed);
                    }
                    other => panic!("{kind}: expected ToolCall, got {other:?}"),
                },
                other => panic!("{kind}: expected ItemStarted, got {other:?}"),
            }
        }
    }

    #[test]
    fn think_tool_calls_become_reasoning() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t4",
            "title": "Thinking",
            "kind": "think",
            "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "step 1" } }]
        })));
        match &events[0] {
            AgentEvent::ItemStarted(item) => match &item.content {
                ItemContent::Reasoning { text } => assert_eq!(text, "step 1"),
                other => panic!("expected Reasoning, got {other:?}"),
            },
            other => panic!("expected ItemStarted, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_status_maps_one_to_one() {
        for (wire, expected) in [
            ("pending", ItemStatus::InProgress),
            ("in_progress", ItemStatus::InProgress),
            ("completed", ItemStatus::Completed),
            ("failed", ItemStatus::Failed),
        ] {
            let mut state = state();
            let events = state.apply_update(update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "s",
                "title": "x",
                "kind": "other",
                "status": wire
            })));
            match &events[0] {
                AgentEvent::ItemStarted(item) => match &item.content {
                    ItemContent::ToolCall { status, .. } => assert_eq!(*status, expected),
                    other => panic!("expected ToolCall, got {other:?}"),
                },
                other => panic!("expected ItemStarted, got {other:?}"),
            }
        }
    }

    /// An update for a tool we have never seen announces it (agents may skip the
    /// initial `tool_call`), and only a terminal status completes the item.
    #[test]
    fn tool_call_update_without_a_prior_tool_call_starts_the_item() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "late",
            "title": "Late tool",
            "kind": "other",
            "status": "in_progress"
        })));
        assert!(matches!(events[0], AgentEvent::ItemStarted(_)));
        let events = state.apply_update(update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "late",
            "status": "in_progress"
        })));
        assert!(matches!(events[0], AgentEvent::ItemUpdated(_)));
    }

    #[test]
    fn plan_replaces_the_step_list() {
        let mut state = state();
        state.turn = Some("turn-1".into());
        let events = state.apply_update(update(json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "Read the code", "priority": "high", "status": "completed" },
                { "content": "Write the fix", "priority": "medium", "status": "in_progress" }
            ]
        })));
        match &events[0] {
            AgentEvent::PlanUpdated {
                turn_id,
                steps,
                explanation,
            } => {
                assert_eq!(turn_id.as_deref(), Some("turn-1"));
                assert!(explanation.is_none());
                assert_eq!(steps.len(), 2);
                assert_eq!(steps[0].step, "Read the code");
                assert_eq!(steps[0].status, PlanStepStatus::Completed);
                assert_eq!(steps[1].status, PlanStepStatus::InProgress);
            }
            other => panic!("expected PlanUpdated, got {other:?}"),
        }
    }

    #[test]
    fn available_commands_become_provider_commands() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                { "name": "review", "description": "Review the diff", "input": null }
            ]
        })));
        match &events[0] {
            AgentEvent::ProviderCommands { commands } => {
                assert_eq!(commands[0].name, "review");
                assert_eq!(commands[0].kind, ProviderCommandKind::Command);
            }
            other => panic!("expected ProviderCommands, got {other:?}"),
        }
    }

    #[test]
    fn usage_update_is_the_context_window() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "usage_update",
            "used": 1200,
            "size": 200000
        })));
        match &events[0] {
            AgentEvent::TokenUsage(usage) => {
                assert_eq!(usage.used_tokens, Some(1200));
                assert_eq!(usage.context_window, Some(200_000));
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn session_info_update_is_ignored() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "session_info_update",
            "title": "the agent's own title"
        })));
        assert!(events.is_empty());
    }

    #[test]
    fn modes_models_and_config_become_provider_options() {
        let mut state = state();
        let modes: acp::SessionModeState = serde_json::from_value(json!({
            "currentModeId": "build",
            "availableModes": [
                { "id": "build", "name": "Build" },
                { "id": "plan", "name": "Plan", "description": "Read-only" }
            ]
        }))
        .unwrap();
        let models: acp::SessionModelState = serde_json::from_value(json!({
            "currentModelId": "sonnet",
            "availableModels": [{ "modelId": "sonnet", "name": "Sonnet" }]
        }))
        .unwrap();
        let config: Vec<acp::SessionConfigOption> = serde_json::from_value(json!([
            {
                "id": "thought_level",
                "name": "Thinking",
                "category": "thought_level",
                "type": "select",
                "currentValue": "medium",
                "options": [
                    { "value": "low", "name": "Low" },
                    { "value": "medium", "name": "Medium" }
                ]
            },
            { "id": "web", "name": "Web search", "type": "boolean", "currentValue": true }
        ]))
        .unwrap();
        state
            .options
            .ingest(Some(&modes), Some(&models), Some(&config));

        let descriptors = state.options.descriptors();
        let ids: Vec<&str> = descriptors.iter().map(descriptor_id).collect();
        assert_eq!(
            ids,
            vec![
                "acp:mode",
                "acp:model",
                "acp:cfg:thought_level",
                "acp:cfg:web"
            ]
        );
        assert_eq!(state.options.origin("acp:mode"), Some(OptionOrigin::Mode));
        assert_eq!(state.options.origin("acp:model"), Some(OptionOrigin::Model));
        assert_eq!(
            state.options.origin("acp:cfg:web"),
            Some(OptionOrigin::Config(acp::SessionConfigId::new("web")))
        );
        assert!(matches!(
            &descriptors[3],
            OptionDescriptor::Boolean {
                default_value: true,
                ..
            }
        ));

        let selections = state.options.selections();
        assert_eq!(selections[0].value, json!("build"));
        assert_eq!(selections[1].value, json!("sonnet"));
        assert_eq!(selections[2].value, json!("medium"));
        assert_eq!(selections[3].value, json!(true));

        // An agent-initiated mode switch re-publishes the options…
        let events = state.apply_update(update(json!({
            "sessionUpdate": "current_mode_update",
            "currentModeId": "plan"
        })));
        match &events[0] {
            AgentEvent::ProviderOptions { selections, .. } => {
                assert_eq!(selections[0].value, json!("plan"));
            }
            other => panic!("expected ProviderOptions, got {other:?}"),
        }

        // …and so does a config-option push.
        let events = state.apply_update(update(json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [
                { "id": "web", "name": "Web search", "type": "boolean", "currentValue": false }
            ]
        })));
        match &events[0] {
            AgentEvent::ProviderOptions { selections, .. } => {
                let web = selections.iter().find(|s| s.id == "acp:cfg:web").unwrap();
                assert_eq!(web.value, json!(false));
            }
            other => panic!("expected ProviderOptions, got {other:?}"),
        }
    }

    #[test]
    fn permission_request_carries_the_agents_own_options() {
        let request: acp::RequestPermissionRequest = serde_json::from_value(json!({
            "sessionId": "s1",
            "toolCall": {
                "toolCallId": "t9",
                "title": "rm -rf build",
                "kind": "execute",
                "rawInput": { "command": "rm -rf build" }
            },
            "options": [
                { "optionId": "yes", "name": "Allow", "kind": "allow_once" },
                { "optionId": "always", "name": "Always allow", "kind": "allow_always" },
                { "optionId": "no", "name": "Reject", "kind": "reject_once" }
            ]
        }))
        .unwrap();
        let approval = approval_request(
            "acp-approval-1".into(),
            Some("turn-1".into()),
            &request.tool_call,
            &request.options,
        );
        match &approval.kind {
            ApprovalKind::ExecCommand { command, .. } => assert_eq!(command, "rm -rf build"),
            other => panic!("expected ExecCommand, got {other:?}"),
        }
        assert_eq!(approval.options.len(), 3);
        assert_eq!(approval.options[0].label, "Allow");
        assert_eq!(approval.options[1].kind, ApprovalOptionKind::AllowAlways);

        // Our fixed four map onto the agent's own options…
        let selected =
            |decision: ApprovalDecision| match approval_outcome(&decision, &approval.options) {
                Some(acp::RequestPermissionOutcome::Selected(outcome)) => {
                    outcome.option_id.0.to_string()
                }
                other => panic!("expected a selection, got {other:?}"),
            };
        assert_eq!(selected(ApprovalDecision::Approve), "yes");
        assert_eq!(selected(ApprovalDecision::ApproveForSession), "always");
        assert_eq!(selected(ApprovalDecision::Deny), "no");
        assert_eq!(
            selected(ApprovalDecision::Option("always".into())),
            "always"
        );
        // …and Cancel is the protocol's own `cancelled` outcome.
        assert!(matches!(
            approval_outcome(&ApprovalDecision::Cancel, &approval.options),
            Some(acp::RequestPermissionOutcome::Cancelled)
        ));

        // The exact wire shape the agent expects back.
        let response = acp::RequestPermissionResponse::new(
            approval_outcome(&ApprovalDecision::Approve, &approval.options).unwrap(),
        );
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["outcome"]["outcome"], "selected");
        assert_eq!(value["outcome"]["optionId"], "yes");
    }

    /// An agent that only offers rejections cannot honor "approve": we cancel
    /// rather than silently picking a rejection.
    #[test]
    fn approve_without_an_allow_option_falls_back_to_cancel() {
        let options = vec![ApprovalOption {
            id: "no".into(),
            label: "Reject".into(),
            kind: ApprovalOptionKind::RejectOnce,
        }];
        assert!(approval_outcome(&ApprovalDecision::Approve, &options).is_none());
    }

    #[test]
    fn approvals_classify_by_tool_kind() {
        let build = |kind: &str| {
            let tool: acp::ToolCallUpdate = serde_json::from_value(json!({
                "toolCallId": "t",
                "title": "Read config",
                "kind": kind,
                "rawInput": { "path": "a.rs" }
            }))
            .unwrap();
            approval_request("a1".into(), None, &tool, &[]).kind
        };
        assert!(matches!(build("read"), ApprovalKind::FileRead { .. }));
        assert!(matches!(build("search"), ApprovalKind::FileRead { .. }));
        assert!(matches!(build("fetch"), ApprovalKind::FileRead { .. }));
        assert!(matches!(build("other"), ApprovalKind::ToolUse { .. }));

        let edit: acp::ToolCallUpdate = serde_json::from_value(json!({
            "toolCallId": "t",
            "title": "Edit",
            "kind": "edit",
            "content": [{ "type": "diff", "path": "a.rs", "oldText": "a\n", "newText": "b\n" }]
        }))
        .unwrap();
        match approval_request("a2".into(), None, &edit, &[]).kind {
            ApprovalKind::FileChange { changes, .. } => assert_eq!(changes[0].path, "a.rs"),
            other => panic!("expected FileChange, got {other:?}"),
        }
    }

    #[test]
    fn stop_reasons_map_to_turn_status() {
        assert_eq!(
            stop_reason_status(acp::StopReason::EndTurn).0,
            TurnStatus::Completed
        );
        assert_eq!(
            stop_reason_status(acp::StopReason::Cancelled).0,
            TurnStatus::Interrupted
        );
        let (status, message) = stop_reason_status(acp::StopReason::Refusal);
        assert_eq!(status, TurnStatus::Failed);
        assert!(message.unwrap().contains("refused"));
        let (status, message) = stop_reason_status(acp::StopReason::MaxTokens);
        assert_eq!(status, TurnStatus::Failed);
        assert!(message.unwrap().contains("token limit"));
    }

    #[test]
    fn paths_outside_the_session_cwd_are_rejected() {
        let state = state();
        assert!(state.resolve_path(Path::new("src/main.rs")).is_ok());
        assert!(
            state
                .resolve_path(Path::new("/tmp/tcode-acp-test/../etc/passwd"))
                .is_err()
        );
        assert!(state.resolve_path(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn prompt_carries_text_and_image_blocks() {
        let blocks = prompt_blocks(
            "hello",
            &[Attachment {
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
            }],
        );
        let value = serde_json::to_value(&blocks).unwrap();
        assert_eq!(value[0]["type"], "text");
        assert_eq!(value[0]["text"], "hello");
        assert_eq!(value[1]["type"], "image");
        assert_eq!(value[1]["mimeType"], "image/png");
        assert_eq!(value[1]["data"], "AAAA");
    }
}
