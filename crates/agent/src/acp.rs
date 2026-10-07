//! Agent Client Protocol provider: any agent from the ACP registry.
//!
//! Registry-managed agents share this adapter; native providers retain their
//! richer protocol-specific clients. The protocol machinery lives in
//! [`crate::acp_session`]; this module is the registry's dialect: launching a
//! registry recipe and the generic policies for agents tcode knows nothing
//! more about.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::v1 as acp;
use serde_json::{Value, json};
use smol::future;

use crate::acp_session::{
    self, Dialect, Established, Launch, Session, Setup, State, TurnEnd, describe, is_auth_required,
    prompt_status,
};
use crate::{
    AcpAgent, AcpLaunch, AgentError, AgentEvent, Attachment, ProviderKind, ResumeCursor,
    SessionHandle, SessionOptions, TokenUsage,
};

/// How long we let an agent's `authenticate` run before giving up and telling
/// the user to sign in with the agent's own CLI (Gemini's is a browser OAuth
/// flow that otherwise blocks session startup for five minutes).
const AUTH_TIMEOUT: Duration = Duration::from_secs(20);

/// Start (or resume) a session with an ACP agent.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    if opts.fork {
        return Err(AgentError::Protocol(
            "session fork is not supported for this provider".into(),
        ));
    }
    let Some(agent) = opts.acp.clone() else {
        return Err(AgentError::Protocol(
            "no ACP agent selected for this session".into(),
        ));
    };
    acp_session::start(ProviderKind::Acp, Registry { agent }, opts).await
}

/// An agent from the registry (or a custom command), driven by the generic
/// ACP policies.
struct Registry {
    agent: AcpAgent,
}

impl Dialect for Registry {
    fn name(&self) -> &str {
        &self.agent.name
    }

    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError> {
        let (program, mut args) = launch_command(&self.agent.launch)?;
        args.extend(opts.extra_args.iter().cloned());
        let env = match &self.agent.launch {
            AcpLaunch::Npx { env, .. }
            | AcpLaunch::Binary { env, .. }
            | AcpLaunch::Custom { env, .. } => env.clone(),
        };
        Ok(Launch {
            program,
            args,
            env,
            client_meta: None,
        })
    }

    async fn establish(&self, setup: &Setup<'_>) -> Result<Established, AgentError> {
        let agent = &self.agent;
        let opts = setup.opts;
        let mcp_servers = setup.mcp_servers().await;
        let resumed = opts
            .resume
            .as_ref()
            .and_then(|cursor| cursor.0.get("acp_session_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|_| setup.init.agent_capabilities.load_session);

        let mut loaded_session = None;
        if let Some(session_id) = resumed {
            // `session/load` replays the whole conversation as `session/update`
            // notifications. Our JSONL log is the authoritative history and the
            // UI has already folded it, so the replay is swallowed (see
            // `session_update`); we only want the session live again.
            let session_id = acp::SessionId::new(session_id);
            let loaded = setup
                .load(
                    acp::LoadSessionRequest::new(session_id.clone(), opts.cwd.clone())
                        .mcp_servers(mcp_servers.clone()),
                )
                .await;
            match loaded {
                Ok(loaded) => {
                    loaded_session = Some((session_id, loaded.modes, loaded.config_options));
                }
                Err(err) => {
                    log::warn!(
                        "acp[{}]: session/load failed ({}); starting a fresh session",
                        agent.id,
                        describe(&err)
                    );
                    setup
                        .warn(format!(
                            "{} could not resume the previous conversation; starting a new one",
                            agent.name
                        ))
                        .await;
                }
            }
        }
        let (session_id, modes, config_options) = match loaded_session {
            Some(session) => session,
            None => {
                let new = new_session(agent, setup, &mcp_servers).await?;
                (new.session_id, new.modes, new.config_options)
            }
        };
        setup.adopt(modes.as_ref(), config_options.as_deref());

        Ok(Established {
            resume: ResumeCursor(json!({
                "acp_session_id": session_id.0.to_string(),
                "acp_agent_id": agent.id,
            })),
            session_id,
        })
    }

    fn session_update(
        &self,
        state: &mut State,
        notification: acp::SessionNotification,
    ) -> Vec<AgentEvent> {
        // While `session/load` replays the conversation we already have on disk,
        // swallow everything: our JSONL log is the source of truth and the
        // timeline was folded from it before the process even started.
        if state.loading() {
            return Vec::new();
        }
        state.apply_update(notification.update)
    }

    fn turn_end(
        &self,
        state: &mut State,
        result: Result<acp::PromptResponse, acp::Error>,
    ) -> TurnEnd {
        let (status, message) = prompt_status(&result);
        let usage = result
            .ok()
            .and_then(|response| response.usage)
            .map(|usage| TokenUsage {
                freshness: crate::ContextFreshness::Current,
                total_processed_tokens: Some(usage.total_tokens),
                input_tokens: Some(usage.input_tokens),
                cached_input_tokens: usage.cached_read_tokens,
                output_tokens: Some(usage.output_tokens),
                ..TokenUsage::default()
            })
            // Fall back to the live context-window figure from `usage_update`.
            .or_else(|| state.usage());
        TurnEnd {
            status,
            message,
            usage,
        }
    }

    async fn steer(
        &self,
        _session: &Session,
        _request_id: String,
        _text: String,
        _attachments: Vec<Attachment>,
    ) {
    }
}

/// The resolved command line for a launch recipe.
///
/// `Npx` becomes `npm exec --yes -- <package> <args…>` (the registry's own
/// contract); `Binary` / `Custom` run as given.
fn launch_command(launch: &AcpLaunch) -> Result<(PathBuf, Vec<String>), AgentError> {
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

async fn new_session(
    agent: &AcpAgent,
    setup: &Setup<'_>,
    mcp_servers: &[acp::McpServer],
) -> Result<acp::NewSessionResponse, AgentError> {
    let (connection, opts, init) = (setup.connection, setup.opts, setup.init);
    let request =
        || acp::NewSessionRequest::new(opts.cwd.clone()).mcp_servers(mcp_servers.to_vec());
    match connection.send_request(request()).block_task().await {
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
            let Some(method_id) = auth_method_id(&method) else {
                return Err(AgentError::Provider(auth_hint(agent, init)));
            };
            log::info!(
                "acp[{}]: session/new needs auth; trying method `{}`",
                agent.id,
                method_id.0
            );
            let authenticated = future::or(
                async {
                    Some(
                        connection
                            .send_request(acp::AuthenticateRequest::new(method_id.clone()))
                            .block_task()
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
            connection
                .send_request(request())
                .block_task()
                .await
                .map_err(|err| {
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
        .or_else(|| {
            methods
                .iter()
                .find(|method| auth_method_id(method).is_some())
        })
        .cloned()
}

fn auth_method_id(method: &acp::AuthMethod) -> Option<acp::AuthMethodId> {
    match method {
        acp::AuthMethod::Agent(method) => Some(method.id.clone()),
        acp::AuthMethod::EnvVar(method) => Some(method.id.clone()),
        acp::AuthMethod::Terminal(method) => Some(method.id.clone()),
        _ => None,
    }
}

/// The message shown when an agent demands credentials we cannot supply.
fn auth_hint(agent: &AcpAgent, init: &acp::InitializeResponse) -> String {
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
