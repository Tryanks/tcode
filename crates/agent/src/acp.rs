//! Agent Client Protocol provider: any agent from the ACP registry.
//!
//! Registry-managed agents share this adapter; native providers retain their
//! richer protocol-specific clients. The protocol machinery lives in
//! [`crate::acp_session`]; this module is the registry's dialect: launching a
//! registry recipe and the generic policies for agents tcode knows nothing
//! more about.

use std::path::PathBuf;

use agent_client_protocol::v1::schema as acp;
use serde_json::{Value, json};

use crate::acp_session::{
    self, Dialect, Established, Launch, Session, Setup, State, TurnEnd, describe, is_auth_required,
    prompt_status,
};
use crate::{
    AcpAgent, AcpCatalog, AcpLaunch, AgentError, AgentEvent, Attachment, LaunchEnv, ProviderKind,
    ResumeCursor, SessionHandle, SessionOptions, SessionProcesses, TokenUsage,
};

/// Start (or resume) a session with an ACP agent.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    if opts.fork.is_some() {
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

/// What `agent` offers a new session, which it says only in `session/new`:
/// `initialize`, then `session/new` with no MCP servers, then `session/close`
/// when the agent can close sessions. The agent runs in a directory made for
/// the probe and removed after it, so no project's own configuration (its
/// hooks, its MCP servers) is loaded. An agent that needs signing in is not
/// asked to: its interactive sign-in belongs to a thread the user starts.
/// The processes it starts are held in `processes`, which stops them.
pub async fn probe_catalog(
    agent: AcpAgent,
    extra_args: Vec<String>,
    launch_env: LaunchEnv,
    processes: &SessionProcesses,
) -> Result<AcpCatalog, AgentError> {
    let cwd = std::env::temp_dir().join(format!("tcode-acp-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).map_err(|err| {
        AgentError::Spawn(format!(
            "could not make a directory to ask `{}` for its models: {err}",
            agent.name
        ))
    })?;
    let opts = SessionOptions {
        cwd: cwd.clone(),
        model: None,
        resume: None,
        fork: None,
        binary_path: None,
        option_selections: Vec::new(),
        mcp_servers: Vec::new(),
        launch_env,
        extra_args,
        acp: Some(agent.clone()),
        instructions: None,
        processes: processes.clone(),
        clock: Default::default(),
        worktrees: Vec::new(),
    };
    let session_cwd = cwd.clone();
    let probed = acp_session::query(
        ProviderKind::Acp,
        Registry {
            agent: agent.clone(),
        },
        opts,
        async move |connection, init| {
            let new = connection
                .send_request(acp::NewSessionRequest::new(session_cwd))
                .block_task()
                .await
                .map_err(|err| {
                    if is_auth_required(&err) {
                        AgentError::Provider(auth_hint(&agent, init))
                    } else {
                        AgentError::Protocol(format!(
                            "`{}` could not start a session to list its models: {}",
                            agent.name,
                            describe(&err)
                        ))
                    }
                })?;
            if init.agent_capabilities.session_capabilities.close.is_some()
                && let Err(err) = connection
                    .send_request(acp::CloseSessionRequest::new(new.session_id.clone()))
                    .block_task()
                    .await
            {
                log::debug!(
                    "acp[{}]: session/close after the probe failed: {}",
                    agent.id,
                    describe(&err)
                );
            }
            Ok(acp_session::catalog(
                ProviderKind::Acp,
                new.modes.as_ref(),
                new.config_options.as_deref(),
            ))
        },
    )
    .await;
    // The agent is gone by now: `query` waits for it, or stopped it.
    if let Err(err) = crate::process::unblock(move || std::fs::remove_dir_all(&cwd)).await {
        log::warn!("could not remove the ACP probe's directory: {err}");
    }
    probed
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
            .map(str::to_string);

        let mut loaded_session = None;
        if let Some(session_id) = resumed {
            // Tcode's own log is the history and the UI has already folded
            // it, so the session is only made live again: what a
            // `session/load` replays is swallowed (see `session_update`).
            let session_id = acp::SessionId::new(session_id);
            match setup.reattach(&session_id, &mcp_servers, None).await {
                Some(Ok((modes, config_options))) => {
                    loaded_session = Some((session_id, modes, config_options));
                }
                Some(Err(err)) => {
                    log::warn!(
                        "acp[{}]: the session could not be resumed ({}); starting a fresh session",
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
                None => {}
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
        let mut config_options = config_options.unwrap_or_default();
        if let Some(model) = &opts.model
            && let Some(selected) = setup
                .select_model(&session_id, model, &config_options)
                .await
        {
            config_options = selected;
        }
        setup
            .restore_options(&session_id, modes.as_ref(), &config_options)
            .await;

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
        result: Result<acp::PromptResponse, agent_client_protocol::Error>,
    ) -> TurnEnd {
        let (status, message) = prompt_status(&result);
        // The prompt result counts the session's tokens; how much of the
        // window is in use is only ever what the last `usage_update` said.
        let context = state.usage();
        let usage = result
            .ok()
            .and_then(|response| response.usage)
            .map(|usage| TokenUsage {
                freshness: crate::ContextFreshness::Current,
                total_processed_tokens: Some(usage.total_tokens),
                input_tokens: Some(usage.input_tokens),
                cached_input_tokens: usage.cached_read_tokens,
                output_tokens: Some(usage.output_tokens),
                used_tokens: context.and_then(|context| context.used_tokens),
                context_window: context.and_then(|context| context.context_window),
                cost_usd: context.and_then(|context| context.cost_usd),
                ..TokenUsage::default()
            })
            .or(context);
        TurnEnd {
            status,
            message,
            usage,
        }
    }

    async fn steer(
        &self,
        _session: &Session,
        _session_id: &acp::SessionId,
        _request_id: String,
        _text: String,
        _attachments: Vec<Attachment>,
    ) {
    }

    /// The model picker lists the agent's models ([`probe_catalog`]), so its
    /// model option is the thread's model rather than one of its options.
    fn owned_config_options(&self) -> &'static [&'static str] {
        &[acp_session::MODEL_OPTION_ID]
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
            // The agent wants credentials. Try its own `authenticate` once:
            // Gemini's is a browser sign-in, which runs for as long as the
            // user takes, or until they stop the thread's start. On failure we
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
            if let Err(err) = connection
                .send_request(acp::AuthenticateRequest::new(method_id.clone()))
                .block_task()
                .await
            {
                return Err(AgentError::Provider(format!(
                    "{} (authentication via `{}` failed: {})",
                    auth_hint(agent, init),
                    method_id.0,
                    describe(&err)
                )));
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

/// Which auth method to drive over the protocol: the first the agent
/// handles itself. A `terminal` method is the agent's own interactive program
/// and is never passed to `authenticate`.
fn preferred_auth_method(methods: &[acp::AuthMethod]) -> Option<acp::AuthMethod> {
    methods
        .iter()
        .find(|method| auth_method_id(method).is_some())
        .cloned()
}

fn auth_method_id(method: &acp::AuthMethod) -> Option<acp::AuthMethodId> {
    match method {
        acp::AuthMethod::Agent(method) => Some(method.id.clone()),
        _ => None,
    }
}

/// The message shown when an agent demands credentials we cannot supply.
fn auth_hint(agent: &AcpAgent, init: &acp::InitializeResponse) -> String {
    let methods: Vec<String> = init
        .auth_methods
        .iter()
        .map(|method| method.name().to_owned())
        .collect();
    let offered = if methods.is_empty() {
        String::new()
    } else {
        format!(" It offers: {}.", methods.join("; "))
    };
    format!(
        "`{}` requires authentication.{offered} Sign in with the agent's own CLI, or set its API-key environment variables in Settings → Harnesses → {}.",
        agent.name, agent.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ACP's `session/prompt` result counts tokens (`PromptResponse.usage`,
    /// unstable `end_turn_token_usage`); how much of the window is in use is
    /// `usage_update`'s `used` and `size` (agent-client-protocol-schema 1.18.0,
    /// `v1/agent.rs` `Usage`, `v1/client.rs` `UsageUpdate`). The turn's usage
    /// replaces the session's, so it carries the window on.
    #[test]
    fn a_turn_end_keeps_the_window_the_last_usage_update_reported() {
        let registry = Registry {
            agent: AcpAgent {
                id: "agent".into(),
                name: "Agent".into(),
                launch: AcpLaunch::Custom {
                    command: "agent".into(),
                    args: Vec::new(),
                    env: Vec::new(),
                },
            },
        };
        let mut state = State::new(ProviderKind::Acp, PathBuf::from("/workspace"));
        state.apply_update(
            serde_json::from_value(
                json!({"sessionUpdate": "usage_update", "used": 1200, "size": 200000}),
            )
            .unwrap(),
        );
        let response = serde_json::from_value(json!({
            "stopReason": "end_turn",
            "usage": {"totalTokens": 9000, "inputTokens": 8000, "outputTokens": 1000}
        }))
        .unwrap();
        let usage = registry.turn_end(&mut state, Ok(response)).usage.unwrap();
        assert_eq!(usage.used_tokens, Some(1200));
        assert_eq!(usage.context_window, Some(200_000));
        assert_eq!(usage.input_tokens, Some(8000));
        assert_eq!(usage.total_processed_tokens, Some(9000));
    }
}
