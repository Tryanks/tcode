//! Google Antigravity CLI (`agy`): `agy acp [--process-per-session] [flags]`,
//! an ACP agent with Antigravity's `_meta.antigravity` extensions, session
//! resumption, steering, and headless `/usage` quota queries.
//!
//! The protocol machinery is [`crate::acp_session`]; this module is
//! Antigravity's dialect.

pub(crate) mod plugins;

use std::path::PathBuf;

use agent_client_protocol::v1::schema as acp;
use serde_json::{Map, Value, json};

use crate::acp_session::{
    self, Dialect, Established, Launch, Session, Setup, State, TurnEnd, describe, is_auth_required,
    prompt_status,
};
use crate::{
    AgentError, AgentEvent, Attachment, LaunchEnv, ModelSpec, ProviderKind, ResumeCursor,
    SessionHandle, SessionOptions, SessionProcesses, TokenUsage,
};

/// The directories the Antigravity CLI launched with `launch_env` keeps its
/// configuration and conversation state in: `~/.gemini/antigravity-cli`.
pub fn state_dirs(launch_env: &LaunchEnv) -> Vec<PathBuf> {
    launch_env
        .child_home(ProviderKind::Antigravity)
        .map(|home| home.join(".gemini").join("antigravity-cli"))
        .into_iter()
        .collect()
}

/// The user-level MCP configuration file Antigravity CLI documents:
/// `~/.gemini/config/mcp_config.json`.
pub fn mcp_config_path(launch_env: &LaunchEnv) -> Option<PathBuf> {
    launch_env
        .child_home(ProviderKind::Antigravity)
        .map(|home| home.join(".gemini").join("config").join("mcp_config.json"))
}

/// Start (or resume) an Antigravity session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    if opts.fork.is_some() {
        return Err(AgentError::Protocol(
            "session fork is not supported for Antigravity".into(),
        ));
    }
    acp_session::start(ProviderKind::Antigravity, Antigravity, opts).await
}

/// The models Antigravity's `session/new` advertises for the signed-in account.
/// Their options (`thought_level`, `mode`) arrive over the wire once a session
/// starts ([`crate::OptionDescriptors::Wire`]).
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
    processes: &SessionProcesses,
) -> Result<Vec<ModelSpec>, AgentError> {
    let cwd = std::env::temp_dir().join(format!("tcode-agy-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).map_err(|err| {
        AgentError::Spawn(format!(
            "could not make a directory to ask Antigravity for its models: {err}"
        ))
    })?;
    let opts = SessionOptions {
        cwd: cwd.clone(),
        model: None,
        resume: None,
        fork: None,
        binary_path,
        option_selections: Vec::new(),
        mcp_servers: Vec::new(),
        launch_env,
        extra_args: Vec::new(),
        acp: None,
        instructions: None,
        processes: processes.clone(),
        clock: Default::default(),
        worktrees: Vec::new(),
    };
    let session_cwd = cwd.clone();
    let listed = acp_session::query(
        ProviderKind::Antigravity,
        Antigravity,
        opts,
        async move |connection, init| {
            let new = connection
                .send_request(acp::NewSessionRequest::new(session_cwd))
                .block_task()
                .await
                .map_err(|err| {
                    if is_auth_required(&err) {
                        signed_out()
                    } else {
                        AgentError::Protocol(format!(
                            "Antigravity could not list its models: {}",
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
                    "antigravity: session/close after model listing failed: {}",
                    describe(&err)
                );
            }
            Ok(acp_session::catalog(
                ProviderKind::Antigravity,
                new.modes.as_ref(),
                new.config_options.as_deref(),
            )
            .models)
        },
    )
    .await;
    if let Err(err) = crate::process::unblock(move || std::fs::remove_dir_all(&cwd)).await {
        log::warn!("could not remove Antigravity's model-probe directory: {err}");
    }
    listed
}

/// Ask a short-lived `agy --print /usage --output-format json` process for
/// account quota windows (handled locally by the CLI with zero model turns).
pub async fn read_usage(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Value, AgentError> {
    let binary = crate::resolve_binary(binary_path.as_deref(), "agy")?;
    let binary_display = binary.to_string_lossy().into_owned();
    let mut cmd = crate::process::command(&binary);
    cmd.arg("--print")
        .arg("/usage")
        .arg("--output-format")
        .arg("json")
        .current_dir(std::env::temp_dir());
    for (key, value) in launch_env.pairs(ProviderKind::Antigravity) {
        cmd.env(key, value);
    }
    let output = crate::process::tree_output(cmd)
        .await
        .map_err(|err| AgentError::Spawn(format!("spawning `{binary_display}`: {err}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let message = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            output.status.to_string()
        };
        return Err(AgentError::Provider(message));
    }
    serde_json::from_str(&stdout).map_err(|err| {
        AgentError::Protocol(format!(
            "Antigravity usage response was not valid JSON: {err}"
        ))
    })
}

fn signed_out() -> AgentError {
    AgentError::Provider(
        "Antigravity is not signed in. Run `agy acp --login`, or set GEMINI_API_KEY in Settings → Harnesses → Antigravity, then try again."
            .into(),
    )
}

/// Build `agy acp` launch arguments: `--process-per-session` is passed when
/// HTTP MCP servers are attached (as Antigravity's ACP docs specify) unless
/// already supplied in `extra_args`.
pub(crate) fn launch_args(has_mcp_servers: bool, extra_args: &[String]) -> Vec<String> {
    let mut args = vec!["acp".to_string()];
    if has_mcp_servers && !extra_args.iter().any(|arg| arg == "--process-per-session") {
        args.push("--process-per-session".to_string());
    }
    args.extend(extra_args.iter().cloned());
    args
}

/// Build the `_meta` object for `session/new` and `session/resume` carrying
/// `_meta.antigravity.systemPrompt.append` when the server advertises it.
pub(crate) fn session_meta(
    init: &acp::InitializeResponse,
    instructions: Option<&str>,
) -> Option<acp::Meta> {
    let instructions = instructions.filter(|text| !text.trim().is_empty())?;
    let supports_append = init
        .agent_capabilities
        .meta
        .as_ref()
        .and_then(|meta| meta.get("antigravity"))
        .and_then(|ag| ag.get("systemPrompt"))
        .and_then(|sp| sp.get("append"))
        .and_then(Value::as_bool)
        == Some(true);
    if !supports_append {
        return None;
    }
    let mut meta = Map::new();
    meta.insert(
        "antigravity".to_string(),
        json!({ "systemPrompt": { "append": instructions } }),
    );
    Some(meta)
}

struct Antigravity;

impl Dialect for Antigravity {
    fn name(&self) -> &str {
        "Antigravity"
    }

    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError> {
        let program = crate::resolve_binary(opts.binary_path.as_deref(), "agy")?;
        let args = launch_args(!opts.mcp_servers.is_empty(), &opts.extra_args);
        Ok(Launch {
            program,
            args,
            env: Vec::new(),
            client_meta: None,
        })
    }

    fn accepts_http_mcp(&self, _init: &acp::InitializeResponse) -> bool {
        // `agy acp` omits `mcpCapabilities.http` from `initialize` yet connects
        // to HTTP `mcpServers` passed in `session/new` and `session/resume`.
        true
    }

    async fn establish(&self, setup: &Setup<'_>) -> Result<Established, AgentError> {
        let opts = setup.opts;
        let mcp_servers = setup.mcp_servers().await;
        let meta = session_meta(setup.init, opts.instructions.as_deref());
        let resumed_id = opts
            .resume
            .as_ref()
            .and_then(|cursor| {
                cursor
                    .0
                    .get("session_id")
                    .or_else(|| cursor.0.get("acp_session_id"))
            })
            .and_then(Value::as_str)
            .map(str::to_string);

        let mut established_session = None;
        if let Some(session_id) = resumed_id {
            let session_id = acp::SessionId::new(session_id);
            if setup
                .init
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some()
            {
                let mut request =
                    acp::ResumeSessionRequest::new(session_id.clone(), opts.cwd.clone())
                        .mcp_servers(mcp_servers.clone());
                if let Some(meta) = meta.clone() {
                    request = request.meta(meta);
                }
                match setup.connection.send_request(request).block_task().await {
                    Ok(resumed) => {
                        established_session =
                            Some((session_id, resumed.modes, resumed.config_options));
                    }
                    Err(err) => {
                        log::warn!(
                            "antigravity: session/resume failed ({}); starting a fresh session",
                            describe(&err)
                        );
                        setup
                            .warn(
                                "Antigravity could not resume the previous conversation; starting a new one"
                                    .into(),
                            )
                            .await;
                    }
                }
            } else if setup.init.agent_capabilities.load_session {
                let mut request =
                    acp::LoadSessionRequest::new(session_id.clone(), opts.cwd.clone())
                        .mcp_servers(mcp_servers.clone());
                if let Some(meta) = meta.clone() {
                    request = request.meta(meta);
                }
                match setup.load(request).await {
                    Ok(loaded) => {
                        established_session =
                            Some((session_id, loaded.modes, loaded.config_options));
                    }
                    Err(err) => {
                        log::warn!(
                            "antigravity: session/load failed ({}); starting a fresh session",
                            describe(&err)
                        );
                        setup
                            .warn(
                                "Antigravity could not resume the previous conversation; starting a new one"
                                    .into(),
                            )
                            .await;
                    }
                }
            }
        }

        let (session_id, modes, config_options) = match established_session {
            Some(session) => session,
            None => {
                let mut request =
                    acp::NewSessionRequest::new(opts.cwd.clone()).mcp_servers(mcp_servers.clone());
                if let Some(meta) = meta {
                    request = request.meta(meta);
                }
                let new = setup
                    .connection
                    .send_request(request)
                    .block_task()
                    .await
                    .map_err(|err| {
                        if is_auth_required(&err) {
                            signed_out()
                        } else {
                            AgentError::Protocol(format!(
                                "Antigravity could not start a session: {}",
                                describe(&err)
                            ))
                        }
                    })?;
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
                "session_id": session_id.0.to_string(),
            })),
            session_id,
        })
    }

    fn session_update(
        &self,
        state: &mut State,
        notification: acp::SessionNotification,
    ) -> Vec<AgentEvent> {
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
        session: &Session,
        session_id: &acp::SessionId,
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    ) {
        // `agy acp` steers a running turn when a second `session/prompt` arrives
        // on the same session: it immediately answers the superseded prompt with
        // `end_turn` and keeps streaming until the steering prompt completes.
        if session.steer_prompt(session_id, &text, &attachments) {
            session.emit(AgentEvent::SteerAccepted { request_id }).await;
        } else {
            session.emit(AgentEvent::SteerRejected { request_id }).await;
        }
    }

    fn owned_config_options(&self) -> &'static [&'static str] {
        &[acp_session::MODEL_OPTION_ID]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_args_pass_process_per_session_only_when_mcp_servers_are_attached() {
        assert_eq!(launch_args(false, &[]), vec!["acp"]);
        assert_eq!(launch_args(true, &[]), vec!["acp", "--process-per-session"]);
        assert_eq!(
            launch_args(true, &["--sandbox".into()]),
            vec!["acp", "--process-per-session", "--sandbox"]
        );
        assert_eq!(
            launch_args(true, &["--process-per-session".into(), "--sandbox".into()]),
            vec!["acp", "--process-per-session", "--sandbox"]
        );
    }

    /// `_meta.antigravity.systemPrompt.append` is sent only when `initialize`
    /// advertises it (observed in `agy acp` 1.5.13).
    #[test]
    fn session_meta_carries_system_prompt_append_only_when_advertised() {
        let advertised: acp::InitializeResponse = serde_json::from_value(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "_meta": {
                    "antigravity": {
                        "tier": true,
                        "systemPrompt": { "append": true }
                    }
                }
            }
        }))
        .unwrap();
        let unadvertised: acp::InitializeResponse = serde_json::from_value(json!({
            "protocolVersion": 1,
            "agentCapabilities": {}
        }))
        .unwrap();

        let meta = session_meta(&advertised, Some("Use project conventions.")).unwrap();
        assert_eq!(
            meta.get("antigravity"),
            Some(&json!({ "systemPrompt": { "append": "Use project conventions." } }))
        );
        assert!(session_meta(&advertised, Some("   ")).is_none());
        assert!(session_meta(&unadvertised, Some("Use project conventions.")).is_none());
    }
}
