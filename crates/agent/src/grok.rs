//! Grok Build: `grok [--permission-mode M] agent [flags] stdio`, an ACP agent
//! with xAI's `_x.ai/*` dialect.
//!
//! The protocol machinery is [`crate::acp_session`]; this module is Grok's
//! dialect.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use agent_client_protocol::{UntypedMessage, schema::v1 as acp};
use serde_json::{Map, Value, json};

use crate::acp_session::{
    self, Dialect, Established, Launch, Session, Setup, State, TurnEnd, describe, is_auth_required,
    prompt_status,
};
use crate::{
    AgentError, AgentEvent, ApprovalMode, Attachment, InteractionMode, LaunchEnv, ModelSpec,
    ProviderKind, ResumeCursor, SessionHandle, SessionOptions, UserInputOption, UserInputQuestion,
};

/// The non-interactive auth method: it validates `XAI_API_KEY` (or the key in
/// Grok's `config.toml`). The other advertised method signs in through a
/// browser and is never driven from here.
const API_KEY_AUTH_METHOD: &str = "xai.api_key";

const ASK_USER_QUESTION: &str = "_x.ai/ask_user_question";
const INTERJECT: &str = "_x.ai/interject";
const INTERJECTION: &str = "_x.ai/session/interjection";
const FORK: &str = "_x.ai/session/fork";

/// Start (or resume, or fork) a Grok session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    acp_session::start(ProviderKind::Grok, Grok::default(), opts).await
}

/// The models Grok's `initialize` advertises. Their options arrive over the
/// wire once a session starts ([`crate::OptionDescriptors::Wire`]).
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    let opts = SessionOptions {
        cwd: std::env::temp_dir(),
        model: None,
        resume: None,
        fork: false,
        binary_path,
        approval_mode: ApprovalMode::default(),
        option_selections: Vec::new(),
        interaction_mode: InteractionMode::default(),
        mcp_servers: Vec::new(),
        launch_env,
        extra_args: Vec::new(),
        acp: None,
    };
    acp_session::query(
        ProviderKind::Grok,
        Grok::default(),
        opts,
        async |_connection, init| Ok(catalog(init)),
    )
    .await
}

/// `initialize`'s `_meta.modelState`.
fn catalog(init: &acp::InitializeResponse) -> Vec<ModelSpec> {
    let Some(state) = init.meta.as_ref().and_then(|meta| meta.get("modelState")) else {
        return Vec::new();
    };
    let current = state.get("currentModelId").and_then(Value::as_str);
    state
        .get("availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model.get("modelId")?.as_str()?;
            Some(ModelSpec {
                id: id.to_string(),
                display_name: model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                is_default: Some(id) == current,
                options: Vec::new(),
            })
        })
        .collect()
}

#[derive(Default)]
struct Grok {
    /// Steers sent with `_x.ai/interject`, oldest first, until Grok echoes
    /// them back as consumed.
    steers: Mutex<VecDeque<(String, String)>>,
}

impl Dialect for Grok {
    fn name(&self) -> &str {
        "Grok"
    }

    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError> {
        let program = crate::resolve_binary(opts.binary_path.as_deref(), "grok")?;
        let permission_mode = match opts.approval_mode {
            ApprovalMode::Supervised | ApprovalMode::ReadOnly => "default",
            ApprovalMode::AutoAcceptEdits => "acceptEdits",
            ApprovalMode::FullAccess => "bypassPermissions",
        };
        let mut args = vec![
            "--permission-mode".to_string(),
            permission_mode.to_string(),
            "agent".to_string(),
        ];
        if let Some(model) = &opts.model {
            args.extend(["--model".to_string(), model.clone()]);
        }
        args.extend(opts.extra_args.iter().cloned());
        args.push("stdio".to_string());
        Ok(Launch {
            program,
            args,
            env: Vec::new(),
            client_meta: None,
        })
    }

    async fn establish(&self, setup: &Setup<'_>) -> Result<Established, AgentError> {
        let (connection, opts) = (setup.connection, setup.opts);
        let offers_api_key = setup.init.auth_methods.iter().any(|method| {
            let id = match method {
                acp::AuthMethod::Agent(method) => &method.id,
                acp::AuthMethod::EnvVar(method) => &method.id,
                acp::AuthMethod::Terminal(method) => &method.id,
                _ => return false,
            };
            id.0.as_ref() == API_KEY_AUTH_METHOD
        });
        let authenticated = if offers_api_key {
            connection
                .send_request(acp::AuthenticateRequest::new(API_KEY_AUTH_METHOD))
                .block_task()
                .await
                .map(|_| ())
        } else {
            Ok(())
        };
        let auth_failure = |err: &acp::Error| {
            let detail = match &authenticated {
                Err(auth) => describe(auth),
                Ok(()) => describe(err),
            };
            AgentError::Provider(format!(
                "Grok requires authentication: {detail}. Set XAI_API_KEY in Settings → Providers → Grok, or sign in with `grok login`."
            ))
        };

        let mcp_servers = setup.mcp_servers().await;
        let resumed = opts
            .resume
            .as_ref()
            .and_then(|cursor| cursor.str_field(&["session_id"]))
            .map(str::to_string);
        let (session_id, modes, config_options) = match resumed {
            Some(source) => {
                let session_id = if opts.fork {
                    fork(connection, &source, opts).await?
                } else {
                    source
                };
                let resumed = connection
                    .send_request(
                        acp::ResumeSessionRequest::new(session_id.clone(), opts.cwd.clone())
                            .mcp_servers(mcp_servers),
                    )
                    .block_task()
                    .await
                    .map_err(|err| {
                        if is_auth_required(&err) {
                            auth_failure(&err)
                        } else {
                            AgentError::Provider(format!(
                                "Grok could not resume session {session_id}: {}",
                                describe(&err)
                            ))
                        }
                    })?;
                (
                    acp::SessionId::new(session_id),
                    resumed.modes,
                    resumed.config_options,
                )
            }
            None => {
                let created = connection
                    .send_request(
                        acp::NewSessionRequest::new(opts.cwd.clone()).mcp_servers(mcp_servers),
                    )
                    .block_task()
                    .await
                    .map_err(|err| {
                        if is_auth_required(&err) {
                            auth_failure(&err)
                        } else {
                            AgentError::Protocol(format!(
                                "Grok could not start a session: {}",
                                describe(&err)
                            ))
                        }
                    })?;
                (created.session_id, created.modes, created.config_options)
            }
        };
        setup.adopt(modes.as_ref(), config_options.as_deref());
        if opts.interaction_mode == InteractionMode::Plan || setup.in_plan_mode() {
            setup
                .apply_interaction_mode(&session_id, opts.interaction_mode)
                .await;
        }
        Ok(Established {
            resume: ResumeCursor(json!({ "session_id": session_id.0.to_string() })),
            session_id,
        })
    }

    fn session_update(
        &self,
        state: &mut State,
        notification: acp::SessionNotification,
    ) -> Vec<AgentEvent> {
        // `session/load` replays history flagged as such; tcode's own log is
        // the conversation of record.
        let replayed = notification
            .meta
            .as_ref()
            .and_then(|meta| meta.get("isReplay"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if replayed {
            return Vec::new();
        }
        state.apply_update(notification.update)
    }

    fn turn_end(
        &self,
        _state: &mut State,
        result: Result<acp::PromptResponse, acp::Error>,
    ) -> TurnEnd {
        let (status, message) = prompt_status(&result);
        TurnEnd {
            status,
            message,
            usage: None,
        }
    }

    async fn steer(
        &self,
        session: &Session,
        request_id: String,
        text: String,
        _attachments: Vec<Attachment>,
    ) {
        let Some(session_id) = session.id() else {
            return;
        };
        self.steers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back((text.clone(), request_id.clone()));
        let sent = session
            .connection
            .send_request(UntypedMessage {
                method: INTERJECT.into(),
                params: json!({ "sessionId": session_id.0.to_string(), "text": text }),
            })
            .block_task()
            .await;
        if let Err(err) = sent {
            self.steers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|(_, pending)| pending != &request_id);
            session
                .emit(AgentEvent::Warning {
                    message: format!("Grok did not accept the steer: {}", describe(&err)),
                })
                .await;
        }
    }

    async fn set_approval_mode(&self, _session: &Session, _mode: ApprovalMode) {
        // Applied as `--permission-mode` when the runtime restarts the session
        // for the next turn.
    }

    fn handles_request(&self, method: &str) -> bool {
        method == ASK_USER_QUESTION
    }

    async fn request(
        &self,
        session: Session,
        method: String,
        params: Value,
    ) -> Result<Value, acp::Error> {
        debug_assert_eq!(method, ASK_USER_QUESTION);
        let questions = questions(&params)?;
        Ok(match session.ask_user(questions).await {
            Some(answers) => json!({ "outcome": "accepted", "answers": answer_lists(answers) }),
            None => json!({ "outcome": "cancelled" }),
        })
    }

    fn notification(&self, _state: &mut State, method: &str, params: Value) -> Vec<AgentEvent> {
        if method != INTERJECTION {
            log::trace!("grok: {method} {params}");
            return Vec::new();
        }
        let text = params.get("text").and_then(Value::as_str);
        let mut steers = self
            .steers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(position) = steers
            .iter()
            .position(|(sent, _)| Some(sent.as_str()) == text)
        else {
            return Vec::new();
        };
        let (_, request_id) = steers.remove(position).expect("position is in range");
        vec![AgentEvent::SteerAccepted { request_id }]
    }
}

/// Fork `source` into a new Grok session in the same directory.
async fn fork(
    connection: &acp_session::Connection,
    source: &str,
    opts: &SessionOptions,
) -> Result<String, AgentError> {
    let cwd = opts.cwd.to_string_lossy();
    let forked = connection
        .send_request(UntypedMessage {
            method: FORK.into(),
            params: json!({ "sourceSessionId": source, "sourceCwd": cwd, "newCwd": cwd }),
        })
        .block_task()
        .await
        .map_err(|err| {
            AgentError::Provider(format!(
                "Grok could not fork session {source}: {}",
                describe(&err)
            ))
        })?;
    forked
        .get("newSessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| AgentError::Protocol(format!("Grok's fork returned no session: {forked}")))
}

/// The questions of an `_x.ai/ask_user_question` request. Grok keys answers
/// by question text.
fn questions(params: &Value) -> Result<Vec<UserInputQuestion>, acp::Error> {
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(acp::Error::invalid_params)?;
    Ok(questions
        .iter()
        .filter_map(|question| {
            let text = question.get("question")?.as_str()?.to_string();
            Some(UserInputQuestion {
                id: text.clone(),
                header: question
                    .get("header")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                question: text,
                options: question
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|option| {
                        Some(UserInputOption {
                            label: option.get("label")?.as_str()?.to_string(),
                            description: option
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect(),
                multi_select: question
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                prefill: None,
            })
        })
        .collect())
}

/// Grok takes every answer as a list of selected labels.
fn answer_lists(answers: Map<String, Value>) -> Map<String, Value> {
    answers
        .into_iter()
        .map(|(question, answer)| {
            let answer = match answer {
                Value::Array(_) => answer,
                other => Value::Array(vec![other]),
            };
            (question, answer)
        })
        .collect()
}
