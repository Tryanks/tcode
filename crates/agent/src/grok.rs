//! Grok Build: `grok [--permission-mode M] agent [flags] stdio`, an ACP agent
//! with xAI's `_x.ai/*` dialect.
//!
//! The protocol machinery is [`crate::acp_session`]; this module is Grok's
//! dialect.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agent_client_protocol::{UntypedMessage, schema::v1 as acp};
use serde_json::{Map, Value, json};

use crate::acp_session::{
    self, Dialect, Established, Launch, Session, Setup, State, TurnEnd, describe, is_auth_required,
    prompt_status, stop_reason_status,
};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalOptionKind, Attachment, Compaction,
    ForkPoint, ItemContent, LaterHistory, LaunchEnv, ModelSpec, ProviderKind, ResumeCursor,
    RewindConflict, RewindConflictKind, RewindFailure, RewindFailureReason, RewindFile,
    RewindFiles, RewindMode, RewindPreview, RewindPreviewResult, SessionCommand, SessionHandle,
    SessionOptions, TokenUsage, TurnStatus, UserInputOption, UserInputQuestion,
};

/// The directory Grok launched with `launch_env` keeps its configuration and
/// sessions in: `GROK_HOME`, else `~/.grok`.
pub fn home_dir(launch_env: &LaunchEnv) -> Option<PathBuf> {
    launch_env
        .child_path(ProviderKind::Grok, "GROK_HOME")
        .or_else(|| dirs::home_dir().map(|home| home.join(".grok")))
}

/// The non-interactive auth method: it validates `XAI_API_KEY` (or the key in
/// Grok's `config.toml`). The other advertised method signs in through a
/// browser and is never driven from here.
const API_KEY_AUTH_METHOD: &str = "xai.api_key";

const ASK_USER_QUESTION: &str = "_x.ai/ask_user_question";
const EXIT_PLAN_MODE: &str = "_x.ai/exit_plan_mode";
const INTERJECT: &str = "_x.ai/interject";
const INTERJECTION: &str = "_x.ai/session/interjection";
const QUEUE_CHANGED: &str = "_x.ai/queue/changed";
const TASK_BACKGROUNDED: &str = "_x.ai/task_backgrounded";
const TASK_COMPLETED: &str = "_x.ai/task_completed";
const SESSION_NOTIFICATION: &str = "_x.ai/session_notification";
const FORK: &str = "_x.ai/session/fork";
const REWIND_POINTS: &str = "_x.ai/rewind/points";
const REWIND_EXECUTE: &str = "_x.ai/rewind/execute";

pub(crate) mod plugins;

/// Start (or resume, or fork) a Grok session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    let grok = Grok::default();
    acp_session::start(ProviderKind::Grok, grok, opts).await
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
        fork: None,
        binary_path,
        option_selections: Vec::new(),
        mcp_servers: Vec::new(),
        launch_env,
        extra_args: Vec::new(),
        acp: None,
        instructions: None,
        processes: Default::default(),
        clock: Default::default(),
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
    let Some(state) = model_state(init) else {
        return Vec::new();
    };
    let current = state.get("currentModelId").and_then(Value::as_str);
    available_models(state)
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

fn model_state(init: &acp::InitializeResponse) -> Option<&Value> {
    init.meta.as_ref()?.get("modelState")
}

fn available_models(state: &Value) -> impl Iterator<Item = &Value> {
    state
        .get("availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

#[derive(Default)]
struct Grok {
    /// Steers sent with `_x.ai/interject`, oldest first, until Grok echoes
    /// them back as consumed.
    steers: Mutex<VecDeque<(String, String)>>,
    models: Mutex<Models>,
    activity: Mutex<Activity>,
    prompts: Mutex<Prompts>,
}

/// Grok's rewind points are its prompts, numbered from 0 in the order it
/// runs them, its own included; rewinding to N keeps 0..N.
#[derive(Default)]
struct Prompts {
    /// The index the next prompt Grok runs takes; unknown until the session
    /// reported its points, and then no prompt is a target.
    next: Option<usize>,
    /// What each prompt numbered here said, to check the number against the
    /// point Grok lists for it before a rewind.
    texts: HashMap<usize, String>,
}

/// What Grok runs: its prompts, as `_x.ai/queue/changed` reports them, and
/// its background tasks. Grok runs prompts one at a time and starts some
/// itself: when a background task finishes it runs a prompt reporting it,
/// which no `session/prompt` answers.
#[derive(Default)]
struct Activity {
    /// Prompts the client queued, until they run.
    queued: HashSet<String>,
    /// The running prompt the client sent.
    client: Option<String>,
    /// The running prompt Grok started itself; its turn has its id.
    agent: Option<String>,
    /// Shell tool calls whose command went on in the background, by the
    /// task Grok runs it as. Grok answers such a call at once, streams its
    /// later output as running updates and reports its end as the task's.
    backgrounded: HashMap<String, String>,
    background_tasks: usize,
    /// The running background tasks as last listed.
    background_list: Vec<crate::BackgroundTask>,
    /// A drop to no background tasks, withheld while a sent turn is open:
    /// Grok follows that turn with one reporting the finished task, and the
    /// runtime must not take the process for idle in between.
    drained: bool,
}

impl Activity {
    fn sent_turn_open(&self, state: &State) -> bool {
        state.turn().is_some() && state.turn() != self.agent.as_deref()
    }
}

/// What usage reporting needs to know about the session's models.
#[derive(Default)]
struct Models {
    /// `totalContextTokens` by model id.
    windows: HashMap<String, u64>,
    /// The model serving the session.
    current: Option<String>,
    /// The latest model call's usage.
    last_call: Option<TokenUsage>,
}

/// One running task of Grok's `background_tasks` list, with the call that
/// started it when Grok has said which (`backgrounded`, by task).
fn background_task(
    task: &Value,
    backgrounded: &HashMap<String, String>,
) -> Option<crate::BackgroundTask> {
    let id = task.get("task_id").and_then(Value::as_str)?;
    let started = task
        .get("started_at")
        .and_then(Value::as_str)
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .and_then(|at| u64::try_from(at.timestamp_millis()).ok());
    Some(crate::BackgroundTask {
        id: id.to_owned(),
        name: ["command", "description"]
            .into_iter()
            .find_map(|key| task.get(key).and_then(Value::as_str))
            .unwrap_or(id)
            .to_owned(),
        kind: match task.get("kind").and_then(Value::as_str) {
            Some("bash" | "shell") => crate::BackgroundKind::Command,
            Some("agent" | "subagent") => crate::BackgroundKind::Agent,
            _ => crate::BackgroundKind::Other,
        },
        item_id: backgrounded.get(id).cloned(),
        started_at: started.unwrap_or_else(crate::now_unix_millis),
        started_reported: started.is_some(),
    })
}

impl Grok {
    fn models(&self) -> MutexGuard<'_, Models> {
        self.models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn activity(&self) -> MutexGuard<'_, Activity> {
        self.activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A background task ended: the call that started it settles with the
    /// task's output and exit code.
    fn task_completed(&self, state: &mut State, params: &Value) -> Vec<AgentEvent> {
        let Some(task) = params.pointer("/update/task_snapshot") else {
            return Vec::new();
        };
        let Some(call) = task
            .get("task_id")
            .and_then(Value::as_str)
            .and_then(|task_id| self.activity().backgrounded.remove(task_id))
        else {
            return Vec::new();
        };
        let exit_code = task.get("exit_code").and_then(Value::as_i64);
        let status = if exit_code == Some(0) {
            acp::ToolCallStatus::Completed
        } else {
            acp::ToolCallStatus::Failed
        };
        let output = task
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut update = acp::ToolCallUpdate::new(
            call,
            acp::ToolCallUpdateFields::new()
                .status(status)
                .content(vec![acp::ToolCallContent::from(output)])
                .raw_output(json!({"type": "Bash", "exit_code": exit_code})),
        );
        let shell = prepare_tool_update(&mut update);
        let mut events = state.apply_update(acp::SessionUpdate::ToolCallUpdate(update));
        if let Some(shell) = shell {
            events.iter_mut().for_each(|event| shell.render(event));
        }
        events
    }

    fn steers(&self) -> MutexGuard<'_, VecDeque<(String, String)>> {
        self.steers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One model call's `response_completed` usage, whose `input_tokens`
    /// excludes the cached prompt.
    fn call_usage(&self, usage: &Value) -> TokenUsage {
        let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        let prompt = count("input_tokens")
            + count("cache_read_input_tokens")
            + count("cache_creation_input_tokens");
        let mut models = self.models();
        let usage = TokenUsage {
            freshness: crate::ContextFreshness::Current,
            input_tokens: Some(prompt),
            cached_input_tokens: Some(count("cache_read_input_tokens")),
            output_tokens: Some(count("output_tokens")),
            used_tokens: Some(prompt + count("output_tokens")),
            context_window: models.window(),
            ..TokenUsage::default()
        };
        models.last_call = Some(usage);
        usage
    }

    /// A turn's end as `turn_completed` reports it: its stop reason, and the
    /// turn's usage on top of its last model call's.
    fn reported_turn_end(&self, update: &Value) -> TurnEnd {
        let (status, message) = match update.get("stop_reason").and_then(Value::as_str) {
            Some("error") => (
                TurnStatus::Failed,
                Some(
                    update
                        .get("agent_result")
                        .and_then(Value::as_str)
                        .unwrap_or("Grok's turn failed")
                        .to_string(),
                ),
            ),
            Some(reason) => serde_json::from_value::<acp::StopReason>(json!(reason))
                .map(stop_reason_status)
                .unwrap_or_else(|_| {
                    (
                        TurnStatus::Failed,
                        Some(format!("The agent stopped: {reason}")),
                    )
                }),
            None => (TurnStatus::Completed, None),
        };
        let models = self.models();
        let usage = update.get("usage").map(|usage| TokenUsage {
            freshness: crate::ContextFreshness::Current,
            turn_processed_tokens: usage.get("totalTokens").and_then(Value::as_u64),
            context_window: models.window(),
            ..models.last_call.unwrap_or_default()
        });
        TurnEnd {
            status,
            message,
            usage,
        }
    }

    /// A running prompt the client did not queue opens a turn of its own.
    fn queue_changed(&self, state: &mut State, params: &Value) -> Vec<AgentEvent> {
        let mut activity = self.activity();
        for entry in params
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                activity.queued.insert(id.to_string());
            }
        }
        let Some(running) = params.get("runningPromptId").and_then(Value::as_str) else {
            return Vec::new();
        };
        if [&activity.client, &activity.agent]
            .iter()
            .any(|prompt| prompt.as_deref() == Some(running))
        {
            return Vec::new();
        }
        let text = params
            .get("runningText")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if activity.queued.remove(running) {
            activity.client = Some(running.to_string());
            let index = self.number_prompt(text);
            return index
                .zip(state.turn())
                .map(|(index, turn_id)| AgentEvent::TurnCheckpoint {
                    turn_id: turn_id.to_owned(),
                    checkpoint_id: index.to_string(),
                })
                .into_iter()
                .collect();
        }
        activity.agent = Some(running.to_string());
        // A prompt Grok runs itself takes a number too; no message of the
        // user's begins it, so it is no target.
        self.number_prompt(text);
        state.begin_agent_turn(running)
    }

    /// The number of the prompt Grok has started running, once the session's
    /// numbering is known.
    fn number_prompt(&self, text: &str) -> Option<usize> {
        let mut prompts = self.prompts();
        let index = prompts.next?;
        prompts.next = Some(index + 1);
        prompts.texts.insert(index, text.to_owned());
        Some(index)
    }

    /// What rewinding to prompt `checkpoint_id` in `mode` would restore.
    async fn preview(
        &self,
        session: &Session,
        session_id: &acp::SessionId,
        checkpoint_id: &str,
        mode: RewindMode,
    ) -> Result<RewindPreview, RewindFailure> {
        let index = prompt_index(checkpoint_id)?;
        let response = execute_rewind(&session.connection, session_id, index, mode, false).await?;
        dry_run(&response)
    }

    /// Rewind to prompt `checkpoint_id`, stopping a running turn first as
    /// Grok's `/rewind` does (it cancels, then lists the points).
    async fn execute(
        &self,
        session: &Session,
        session_id: &acp::SessionId,
        checkpoint_id: &str,
        mode: RewindMode,
    ) -> Result<AgentEvent, RewindFailure> {
        let index = prompt_index(checkpoint_id)?;
        if session.with_state(|state| state.turn().is_some()) {
            session.interrupt(session_id).await;
            while session.with_state(|state| state.turn().is_some()) {
                smol::Timer::after(Duration::from_millis(25)).await;
            }
        }
        self.check_numbering(session, session_id, index).await?;
        let response = execute_rewind(&session.connection, session_id, index, mode, true).await?;
        let rewound = rewound(&response, checkpoint_id, mode)?;
        if mode.includes_conversation() {
            // The conversation now ends before prompt `index`, which the next
            // prompt takes again.
            let mut prompts = self.prompts();
            prompts.next = Some(index);
            prompts.texts.retain(|number, _| *number < index);
        }
        Ok(rewound)
    }

    /// Refuse a rewind whose number no longer names the prompt it was given
    /// to: Grok lists no such point, or lists another prompt under it.
    async fn check_numbering(
        &self,
        session: &Session,
        session_id: &acp::SessionId,
        index: usize,
    ) -> Result<(), RewindFailure> {
        let points = rewind_points(&session.connection, session_id)
            .await
            .map_err(|err| RewindFailure::new(RewindFailureReason::Other, describe(&err)))?;
        let Some((_, preview)) = points.iter().find(|(number, _)| *number == index) else {
            return Err(RewindFailure::new(
                RewindFailureReason::TargetNotFound,
                format!("Grok lists no rewind point {index}"),
            ));
        };
        let recorded = self.prompts().texts.get(&index).cloned();
        if let (Some(preview), Some(text)) = (preview, recorded) {
            // The picker's preview: the prompt's first line, cut at 60
            // characters with "...".
            let stem = preview.strip_suffix("...").unwrap_or(preview).trim();
            if !text.contains(stem) {
                return Err(RewindFailure::new(
                    RewindFailureReason::ConversationChanged,
                    format!("Grok's rewind point {index} is \"{preview}\""),
                ));
            }
        }
        Ok(())
    }

    fn prompts(&self) -> MutexGuard<'_, Prompts> {
        self.prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `turn_completed` ends the turn of the prompt it names, in order with
    /// everything else Grok sends; a sent turn's later `session/prompt`
    /// result then completes nothing.
    fn turn_completed(&self, state: &mut State, update: &Value) -> Vec<AgentEvent> {
        let Some(prompt) = update.get("prompt_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let end = self.reported_turn_end(update);
        let mut activity = self.activity();
        if activity.agent.as_deref() == Some(prompt) {
            activity.agent = None;
            return state.end_turn(prompt, end);
        }
        if activity.client.as_deref() == Some(prompt) {
            activity.client = None;
            if let Some(turn) = state.turn().map(str::to_string)
                && activity.sent_turn_open(state)
            {
                return state.end_turn(&turn, end);
            }
        }
        Vec::new()
    }

    /// The `session/prompt` result's `_meta`: the last model call's figures
    /// (whose `totalTokens` is the context in use) and the turn's `usage`.
    fn turn_usage(&self, meta: &acp::Meta) -> TokenUsage {
        let count = |value: Option<&Value>| value.and_then(Value::as_u64);
        let mut models = self.models();
        if let Some(model) = meta.get("modelId").and_then(Value::as_str) {
            models.current = Some(model.to_string());
        }
        TokenUsage {
            freshness: crate::ContextFreshness::Current,
            turn_processed_tokens: count(
                meta.get("usage").and_then(|usage| usage.get("totalTokens")),
            ),
            input_tokens: count(meta.get("inputTokens")),
            cached_input_tokens: count(meta.get("cachedReadTokens")),
            output_tokens: count(meta.get("outputTokens")),
            // `/compact` reports 0: nothing has been measured since.
            used_tokens: count(meta.get("totalTokens")).filter(|tokens| *tokens > 0),
            context_window: models.window(),
            ..TokenUsage::default()
        }
    }

    /// One `_x.ai/session_notification` update.
    fn session_notification(&self, state: &mut State, update: &Value) -> Vec<AgentEvent> {
        let count = |key: &str| update.get(key).and_then(Value::as_u64);
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("response_completed") => update
                .get("usage")
                .map(|usage| vec![AgentEvent::TokenUsage(self.call_usage(usage))])
                .unwrap_or_default(),
            // A request that failed for good ends the turn, which reports it.
            Some("retry_state")
                if update.get("type").and_then(Value::as_str) == Some("retrying") =>
            {
                vec![AgentEvent::Error {
                    message: retry_message(update),
                    fatal: false,
                }]
            }
            Some("image_dropped") => {
                let notes: Vec<&str> = update
                    .get("notes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if notes.is_empty() {
                    return Vec::new();
                }
                vec![AgentEvent::Warning {
                    message: notes.join("\n"),
                }]
            }
            Some("background_tasks") => {
                let mut activity = self.activity();
                let listed: Vec<crate::BackgroundTask> = update
                    .get("tasks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|task| task.get("status").and_then(Value::as_str) == Some("running"))
                    .filter_map(|task| background_task(task, &activity.backgrounded))
                    .collect();
                let running = listed.len();
                let list = (listed != activity.background_list).then(|| {
                    activity.background_list = listed.clone();
                    AgentEvent::BackgroundTaskList { tasks: listed }
                });
                let drained = running == 0 && activity.background_tasks > 0;
                activity.background_tasks = running;
                activity.drained = drained && activity.sent_turn_open(state);
                if activity.drained {
                    return list.into_iter().collect();
                }
                list.into_iter()
                    .chain([AgentEvent::BackgroundTasksChanged { count: running }])
                    .collect()
            }
            Some("turn_completed") => self.turn_completed(state, update),
            Some("auto_compact_started") => vec![AgentEvent::ContextCompacted(Compaction {
                in_progress: true,
                trigger: Some("auto".into()),
                pre_tokens: count("tokens_used"),
                ..Compaction::default()
            })],
            Some("auto_compact_completed") => vec![AgentEvent::ContextCompacted(Compaction {
                in_progress: false,
                pre_tokens: count("tokens_before"),
                post_tokens: count("tokens_after"),
                duration_ms: count("elapsed_ms"),
                ..Compaction::default()
            })],
            _ => Vec::new(),
        }
    }

    fn steer_accepted(&self, params: &Value) -> Vec<AgentEvent> {
        let text = params.get("text").and_then(Value::as_str);
        let mut steers = self.steers();
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

impl Models {
    fn window(&self) -> Option<u64> {
        self.windows.get(self.current.as_deref()?).copied()
    }
}

impl Dialect for Grok {
    fn name(&self) -> &str {
        "Grok"
    }

    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError> {
        let program = crate::resolve_binary(opts.binary_path.as_deref(), "grok")?;
        // Live switching through x.ai/yolo_mode_changed is unverified.
        let permission_mode = opts
            .option_selections
            .iter()
            .find(|selection| selection.id == "permissionMode")
            .and_then(|selection| selection.value.as_str())
            .unwrap_or("default");
        let mut args = vec![
            "--permission-mode".to_string(),
            permission_mode.to_string(),
            "agent".to_string(),
        ];
        if let Some(model) = &opts.model {
            args.extend(["--model".to_string(), model.clone()]);
        }
        if let Some(effort) = acp_session::config_selection(
            &opts.option_selections,
            "reasoning_effort",
            Some(&acp::SessionConfigOptionCategory::ThoughtLevel),
        ) {
            args.extend(["--reasoning-effort".to_string(), effort.to_string()]);
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
        if let Some(state) = model_state(setup.init) {
            let mut models = self.models();
            models.windows = available_models(state)
                .filter_map(|model| {
                    let id = model.get("modelId")?.as_str()?;
                    let window = model.pointer("/_meta/totalContextTokens")?.as_u64()?;
                    Some((id.to_string(), window))
                })
                .collect();
            models.current = opts.model.clone().or_else(|| {
                state
                    .get("currentModelId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        }
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
                "Grok requires authentication: {detail}. Set XAI_API_KEY in Settings → Harnesses → Grok, or sign in with `grok login`."
            ))
        };

        let mcp_servers = setup.mcp_servers().await;
        let resumed = opts
            .resume
            .as_ref()
            .and_then(|cursor| cursor.str_field(&["session_id"]))
            .map(str::to_string);
        let resumed_session = resumed.is_some();
        let (session_id, modes, config_options) = match resumed {
            Some(source) => {
                let session_id = match &opts.fork {
                    Some(ForkPoint::Branch { .. }) => {
                        return Err(AgentError::Provider(
                            "Grok keeps no branches to go on in".into(),
                        ));
                    }
                    Some(point) => fork(connection, &source, point, opts).await?,
                    None => source,
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
        // The prompts a resumed or forked session already ran hold the numbers
        // before the next one's. A Grok without rewind points leaves every
        // turn without a restore point.
        let next = if resumed_session {
            rewind_points(connection, &session_id)
                .await
                .ok()
                .map(|points| points.iter().map(|point| point.0 + 1).max().unwrap_or(0))
        } else {
            Some(0)
        };
        self.prompts().next = next;
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
        let mut update = notification.update;
        let mut launched = None;
        let shell = match &mut update {
            acp::SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.0.as_ref();
                if self.activity().backgrounded.values().any(|call| call == id) {
                    // The call's own answer only says the task started, and
                    // its output after that is the task's: it settles when
                    // the task does.
                    let raw_type = update
                        .fields
                        .raw_output
                        .as_ref()
                        .and_then(|raw| raw.get("type"))
                        .and_then(Value::as_str);
                    if raw_type == Some("BackgroundTaskStarted") {
                        launched = Some(id.to_owned());
                    }
                    if matches!(raw_type, Some("Bash" | "BackgroundTaskStarted")) {
                        update.fields.status = Some(acp::ToolCallStatus::InProgress);
                    }
                }
                prepare_tool_update(update)
            }
            _ => None,
        };
        let mut events = state.apply_update(update);
        if let Some(shell) = shell {
            events.iter_mut().for_each(|event| shell.render(event));
        }
        events.extend(launched.map(|item_id| AgentEvent::ItemBackgrounded {
            item_id,
            parent_item_id: None,
        }));
        events
    }

    fn turn_end(
        &self,
        _state: &mut State,
        result: Result<acp::PromptResponse, acp::Error>,
    ) -> TurnEnd {
        let (status, message) = match (prompt_status(&result), &result) {
            ((TurnStatus::Failed, _), Err(err)) => (TurnStatus::Failed, Some(failure_message(err))),
            (outcome, _) => outcome,
        };
        TurnEnd {
            status,
            message,
            usage: result
                .as_ref()
                .ok()
                .and_then(|response| response.meta.as_ref())
                .map(|meta| self.turn_usage(meta)),
        }
    }

    async fn steer(
        &self,
        session: &Session,
        session_id: &acp::SessionId,
        request_id: String,
        text: String,
        _attachments: Vec<Attachment>,
    ) {
        self.steers().push_back((text.clone(), request_id.clone()));
        let sent = session
            .connection
            .send_request(UntypedMessage {
                method: INTERJECT.into(),
                params: json!({ "sessionId": session_id.0.to_string(), "text": text }),
            })
            .block_task()
            .await;
        if let Err(err) = sent {
            self.steers().retain(|(_, pending)| pending != &request_id);
            session
                .emit(AgentEvent::Warning {
                    message: format!("Grok did not accept the steer: {}", describe(&err)),
                })
                .await;
        }
    }

    /// Offered `fs/*` and `terminal/*`, Grok runs every read, write and
    /// command through them instead of its own tools.
    fn client_services(&self) -> bool {
        false
    }

    fn owned_config_options(&self) -> &'static [&'static str] {
        &["model"]
    }

    fn handles_request(&self, method: &str) -> bool {
        matches!(method, ASK_USER_QUESTION | EXIT_PLAN_MODE)
    }

    /// Grok's `/rewind`: `_x.ai/rewind/execute` to a prompt's number, with
    /// `force: false` a dry run that lists the files it would restore and
    /// those changed outside Grok since, and with `force: true` the rewind.
    async fn rewind(
        self: Arc<Self>,
        session: Session,
        session_id: acp::SessionId,
        command: SessionCommand,
    ) -> Option<AgentEvent> {
        match command {
            SessionCommand::PreviewRewind {
                checkpoint_id,
                mode,
            } => Some(AgentEvent::RewindPreviewed {
                result: match self
                    .preview(&session, &session_id, &checkpoint_id, mode)
                    .await
                {
                    Ok(preview) => RewindPreviewResult::Ready(preview),
                    Err(failure) => RewindPreviewResult::Refused(failure),
                },
                checkpoint_id,
                mode,
            }),
            SessionCommand::Rewind {
                checkpoint_id,
                mode,
            } => Some(
                match self
                    .execute(&session, &session_id, &checkpoint_id, mode)
                    .await
                {
                    Ok(event) => event,
                    Err(failure) => AgentEvent::RewindFailed {
                        checkpoint_id,
                        mode,
                        failure,
                    },
                },
            ),
            command => command.rewind_refusal(RewindFailure::new(
                RewindFailureReason::Unsupported,
                "Grok keeps no rewind to undo".to_owned(),
            )),
        }
    }

    async fn request(
        &self,
        session: Session,
        method: String,
        params: Value,
    ) -> Result<Value, acp::Error> {
        if method == EXIT_PLAN_MODE {
            let decision = session
                .ask_plan(
                    &params,
                    "planContent",
                    [
                        ("approve", "Approve", ApprovalOptionKind::AllowOnce),
                        (
                            "keep_planning",
                            "Keep planning",
                            ApprovalOptionKind::RejectOnce,
                        ),
                        ("abandon", "Abandon", ApprovalOptionKind::RejectAlways),
                    ],
                )
                .await;
            return Ok(match decision {
                ApprovalDecision::Option(id) if id == "approve" => json!({"outcome": "approved"}),
                ApprovalDecision::Option(id) if id == "keep_planning" => {
                    json!({"outcome": "cancelled"})
                }
                ApprovalDecision::Feedback(feedback) => {
                    json!({"outcome": "cancelled", "feedback": feedback})
                }
                ApprovalDecision::Option(_) | ApprovalDecision::Cancel => {
                    json!({"outcome": "abandoned"})
                }
            });
        }
        let questions = questions(&params)?;
        Ok(match session.ask_user(questions).await {
            Some(answers) => json!({ "outcome": "accepted", "answers": answer_lists(answers) }),
            None => json!({ "outcome": "cancelled" }),
        })
    }

    fn notification(&self, state: &mut State, method: &str, params: Value) -> Vec<AgentEvent> {
        let mut events = match method {
            INTERJECTION => self.steer_accepted(&params),
            QUEUE_CHANGED => self.queue_changed(state, &params),
            TASK_BACKGROUNDED => {
                let (Some(call), Some(task)) = (
                    params
                        .pointer("/update/tool_call_id")
                        .and_then(Value::as_str),
                    params.pointer("/update/task_id").and_then(Value::as_str),
                ) else {
                    return Vec::new();
                };
                let mut activity = self.activity();
                activity
                    .backgrounded
                    .insert(task.to_owned(), call.to_owned());
                // A task listed before Grok said which call started it.
                let listed = activity
                    .background_list
                    .iter_mut()
                    .find(|listed| listed.id == task && listed.item_id.is_none());
                match listed {
                    Some(listed) => {
                        listed.item_id = Some(call.to_owned());
                        vec![AgentEvent::BackgroundTaskList {
                            tasks: activity.background_list.clone(),
                        }]
                    }
                    None => Vec::new(),
                }
            }
            TASK_COMPLETED => self.task_completed(state, &params),
            SESSION_NOTIFICATION => params
                .get("update")
                .map(|update| self.session_notification(state, update))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let mut activity = self.activity();
        if activity.drained && !activity.sent_turn_open(state) {
            activity.drained = false;
            events.push(AgentEvent::BackgroundTasksChanged { count: 0 });
        }
        events
    }
}

/// Grok fails a turn with `-32603 Internal error`, its own words in `data`.
fn failure_message(err: &acp::Error) -> String {
    err.data
        .as_ref()
        .and_then(|data| data.get("message"))
        .and_then(Value::as_str)
        .map_or_else(|| describe(err), str::to_string)
}

fn retry_message(update: &Value) -> String {
    let reason = update
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("Grok's model request failed");
    match (
        update.get("attempt").and_then(Value::as_u64),
        update.get("max_retries").and_then(Value::as_u64),
    ) {
        (Some(attempt), Some(max)) => format!("{reason} (retry {attempt} of {max})"),
        _ => reason.to_string(),
    }
}

/// Grok's shell tool resends the command's whole output with every update.
struct ShellResult {
    item_id: String,
    output: String,
    exit_code: Option<i32>,
}

impl ShellResult {
    fn render(&self, event: &mut AgentEvent) {
        if let AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item) = event
            && item.id == self.item_id
            && let ItemContent::CommandExecution {
                output, exit_code, ..
            } = &mut item.content
        {
            output.clone_from(&self.output);
            *exit_code = self.exit_code;
        }
    }
}

/// Bring a `tool_call_update` into the shape the standard mapping renders
/// faithfully. Grok's `rawOutput` is its typed tool result, not display text,
/// so it must never become an item's output. Returns the shell result the
/// mapped item shows instead.
fn prepare_tool_update(update: &mut acp::ToolCallUpdate) -> Option<ShellResult> {
    let fields = &mut update.fields;
    let raw_type = fields
        .raw_output
        .as_ref()
        .and_then(|raw| raw.get("type"))
        .and_then(Value::as_str);
    match raw_type {
        Some("Bash") => {
            let finished = matches!(
                fields.status,
                Some(acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed)
            );
            // Running updates carry a placeholder exit code.
            let exit_code = fields
                .raw_output
                .as_ref()
                .and_then(|raw| raw.get("exit_code"))
                .and_then(Value::as_i64)
                .filter(|_| finished)
                .map(|code| code as i32);
            Some(ShellResult {
                item_id: update.tool_call_id.0.to_string(),
                output: text_of(fields.content.as_deref().unwrap_or_default()),
                exit_code,
            })
        }
        Some("MCP") => {
            let text = fields
                .raw_output
                .as_ref()
                .and_then(|raw| raw.pointer("/output/OkayOutput"))
                .map(|output| match output {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                });
            if fields.content.is_none()
                && let Some(text) = text
            {
                fields.content = Some(vec![text.into()]);
            }
            None
        }
        // Before a command runs, its content is the model's description of it.
        None if fields.kind == Some(acp::ToolKind::Execute) => {
            fields.content = None;
            None
        }
        _ => None,
    }
}

fn text_of(content: &[acp::ToolCallContent]) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            acp::ToolCallContent::Content(block) => match &block.content {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Fork `source` into a new Grok session in the same directory, up to the
/// prompt `point` names when it names one. Unlike `_x.ai/rewind/execute`, the
/// fork's `targetPromptIndex` keeps the prompt it names: a fork before a prompt
/// names the one before it.
async fn fork(
    connection: &acp_session::Connection,
    source: &str,
    point: &ForkPoint,
    opts: &SessionOptions,
) -> Result<String, AgentError> {
    let forked = connection
        .send_request(UntypedMessage {
            method: FORK.into(),
            params: fork_params(source, point, opts)?,
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

/// `_x.ai/session/fork`'s params: `source` copied into the same directory,
/// up to the prompt `point` names.
fn fork_params(
    source: &str,
    point: &ForkPoint,
    opts: &SessionOptions,
) -> Result<Value, AgentError> {
    let cwd = opts.cwd.to_string_lossy();
    let mut params = json!({ "sourceSessionId": source, "sourceCwd": cwd, "newCwd": cwd });
    if let ForkPoint::Before { checkpoint_id } = point {
        let index = prompt_index(checkpoint_id)
            .map_err(|failure| AgentError::Protocol(failure.detail.unwrap_or_default()))?;
        params["targetPromptIndex"] = json!(index.checked_sub(1).ok_or_else(|| {
            AgentError::Provider("Grok cannot fork before a session's first prompt".into())
        })?);
    }
    Ok(params)
}

/// Grok's rewind points, `(prompt index, preview)`.
async fn rewind_points(
    connection: &acp_session::Connection,
    session_id: &acp::SessionId,
) -> Result<Vec<(usize, Option<String>)>, acp::Error> {
    let response = connection
        .send_request(UntypedMessage {
            method: REWIND_POINTS.into(),
            params: json!({ "sessionId": session_id.0.to_string() }),
        })
        .block_task()
        .await?;
    Ok(list(&response, "rewind_points")
        .filter_map(|point| {
            let index = point.get("prompt_index")?.as_u64()?;
            let preview = point
                .get("prompt_preview")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Some((usize::try_from(index).ok()?, preview))
        })
        .collect())
}

/// `_x.ai/rewind/execute` to before prompt `index`.
async fn execute_rewind(
    connection: &acp_session::Connection,
    session_id: &acp::SessionId,
    index: usize,
    mode: RewindMode,
    force: bool,
) -> Result<Value, RewindFailure> {
    let mode = match mode {
        RewindMode::FilesAndConversation => "all",
        RewindMode::Conversation => "conversation_only",
        RewindMode::Files => "files_only",
    };
    connection
        .send_request(UntypedMessage {
            method: REWIND_EXECUTE.into(),
            params: json!({
                "sessionId": session_id.0.to_string(),
                "targetPromptIndex": index,
                "force": force,
                "mode": mode,
            }),
        })
        .block_task()
        .await
        .map_err(|err| RewindFailure::new(RewindFailureReason::Other, describe(&err)))
}

/// What a dry `_x.ai/rewind/execute` says a rewind would restore.
fn dry_run(response: &Value) -> Result<RewindPreview, RewindFailure> {
    let conflicts: Vec<RewindConflict> = list(response, "conflicts")
        .filter_map(|conflict| {
            Some(RewindConflict {
                path: conflict.get("path")?.as_str()?.to_owned(),
                kind: conflict
                    .get("conflict_type")
                    .and_then(|kind| serde_json::from_value(kind.clone()).ok())
                    .unwrap_or(RewindConflictKind::Other),
            })
        })
        .collect();
    // A dry run never succeeds; its error is a refusal unless it only
    // names the conflicts it lists.
    if conflicts.is_empty()
        && let Some(error) = response.get("error").and_then(Value::as_str)
    {
        return Err(refusal(error));
    }
    let files = list(response, "clean_files")
        .filter_map(Value::as_str)
        .map(RewindFile::path)
        .chain(
            conflicts
                .iter()
                .map(|conflict| RewindFile::path(&conflict.path)),
        )
        .collect();
    Ok(RewindPreview {
        files: RewindFiles {
            files,
            ..RewindFiles::default()
        },
        conflicts,
    })
}

/// What a forced `_x.ai/rewind/execute` to `checkpoint_id` restored.
fn rewound(
    response: &Value,
    checkpoint_id: &str,
    mode: RewindMode,
) -> Result<AgentEvent, RewindFailure> {
    if response.get("success").and_then(Value::as_bool) != Some(true) {
        let error = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Grok did not rewind");
        return Err(if list(response, "conflicts").next().is_some() {
            RewindFailure::new(RewindFailureReason::Conflicts, error.to_owned())
        } else {
            refusal(error)
        });
    }
    Ok(AgentEvent::RewindCompleted {
        checkpoint_id: checkpoint_id.to_owned(),
        mode,
        prefill: response
            .get("prompt_text")
            .and_then(Value::as_str)
            .filter(|_| mode.includes_conversation())
            .map(str::to_owned),
        prefill_attachments: Vec::new(),
        later_history: LaterHistory::Truncated,
        files: mode.includes_files().then(|| RewindFiles {
            files: list(response, "reverted_files")
                .filter_map(Value::as_str)
                .map(RewindFile::path)
                .collect(),
            ..RewindFiles::default()
        }),
        incomplete: None,
        undo_token: None,
    })
}

fn list<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn prompt_index(checkpoint_id: &str) -> Result<usize, RewindFailure> {
    checkpoint_id.parse().map_err(|_| {
        RewindFailure::new(
            RewindFailureReason::TargetNotFound,
            format!("`{checkpoint_id}` is not a Grok prompt number"),
        )
    })
}

/// A rewind Grok refused, in its words.
fn refusal(error: &str) -> RewindFailure {
    let reason = if error.starts_with("Cannot rewind to prompt") {
        RewindFailureReason::TargetNotFound
    } else {
        RewindFailureReason::Other
    };
    RewindFailure::new(reason, error.to_owned())
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{ContextFreshness, FileChangeKind, ItemStatus, OptionSelection, ThreadItem};

    // Grok 1.0.46 sessions recorded on the wire in both directions
    // (provenance: `tests/fixtures/grok/README.md`).
    /// A resumed session under AutoAcceptEdits.
    const RESUMED_TURN: &str = include_str!("../tests/fixtures/grok/resumed_turn.jsonl");
    /// A background task finishing during a sent turn, the turn Grok starts
    /// to report it, then another sent turn.
    const AGENT_STARTED_TURN: &str =
        include_str!("../tests/fixtures/grok/agent_started_turn.jsonl");

    fn records(fixture: &str) -> impl Iterator<Item = (bool, Value)> + '_ {
        fixture.lines().map(|line| {
            let mut record: Value = serde_json::from_str(line).unwrap();
            (record["from"] == "client", record["message"].take())
        })
    }

    /// The agent's side of a recording fed to Grok's dialect in wire order,
    /// routed as the ACP session routes it: a `session/update` the schema
    /// reads goes to `session_update`, every other notification to
    /// `notification`. Each `session/prompt` the client sent opens a turn,
    /// and its result settles that turn unless Grok's own `turn_completed`
    /// already has. Grok's requests to the user are left out.
    fn replay(fixture: &str) -> Vec<AgentEvent> {
        let grok = Grok::default();
        let mut state = State::new(std::env::temp_dir());
        let mut prompts = HashMap::new();
        let mut events = Vec::new();
        for (from_client, message) in records(fixture) {
            let method = message.get("method").and_then(Value::as_str);
            if from_client {
                if method == Some("session/prompt") {
                    let turn = format!("turn-{}", prompts.len() + 1);
                    events.extend(state.begin_agent_turn(&turn));
                    prompts.insert(message["id"].to_string(), turn);
                }
                continue;
            }
            match method {
                Some(_) if message.get("id").is_some() => {}
                Some(method) => {
                    let params = message["params"].clone();
                    let update = (method == "session/update")
                        .then(|| serde_json::from_value::<acp::SessionNotification>(params.clone()))
                        .and_then(Result::ok);
                    events.extend(match update {
                        Some(update) => grok.session_update(&mut state, update),
                        None => grok.notification(&mut state, method, params),
                    });
                }
                None => {
                    let Some(turn) = prompts.get(&message["id"].to_string()) else {
                        continue;
                    };
                    if state.turn() == Some(turn.as_str()) {
                        let response = serde_json::from_value(message["result"].clone()).unwrap();
                        let end = grok.turn_end(&mut state, Ok(response));
                        events.extend(state.end_turn(turn, end));
                    }
                }
            }
        }
        events
    }

    /// The client's answer to the agent's request `method` in a recording.
    fn exchange(fixture: &str, method: &str) -> (Value, Value) {
        let mut records = records(fixture);
        let (_, request) = records
            .find(|(from_client, message)| !from_client && message["method"] == method)
            .unwrap_or_else(|| panic!("no {method} in the recording"));
        let (_, answer) = records
            .find(|(from_client, message)| *from_client && message["id"] == request["id"])
            .unwrap();
        (request["params"].clone(), answer["result"].clone())
    }

    fn command_outputs<'a>(events: &'a [AgentEvent], id: &str) -> Vec<(&'a str, Option<i32>)> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item)
                    if item.id == id =>
                {
                    match &item.content {
                        ItemContent::CommandExecution {
                            output, exit_code, ..
                        } => Some((output.as_str(), *exit_code)),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect()
    }

    fn completed<'a>(events: &'a [AgentEvent], id: &str) -> &'a ItemContent {
        events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ItemCompleted(ThreadItem {
                    id: item, content, ..
                }) if item == id => Some(content),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{id} never completed: {events:#?}"))
    }

    fn completed_last<'a>(events: &'a [AgentEvent], id: &str) -> &'a ItemContent {
        events
            .iter()
            .rev()
            .find_map(|event| match event {
                AgentEvent::ItemCompleted(ThreadItem {
                    id: item, content, ..
                }) if item == id => Some(content),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{id} never completed: {events:#?}"))
    }

    fn streamed(events: &[AgentEvent], id: &str) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Delta { item_id, text, .. } if item_id == id => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The fixtures' README records what Grok was launched with for the
    /// recorded session.
    #[test]
    fn a_session_launches_grok_as_it_was_recorded() {
        let opts = SessionOptions {
            cwd: std::env::temp_dir(),
            model: None,
            resume: None,
            fork: None,
            binary_path: Some(PathBuf::from("/opt/grok/bin/grok")),
            option_selections: vec![OptionSelection {
                id: crate::EFFORT_OPTION_ID.into(),
                value: json!("low"),
            }],
            mcp_servers: Vec::new(),
            launch_env: LaunchEnv::default(),
            extra_args: Vec::new(),
            acp: None,
            instructions: None,
            processes: Default::default(),
            clock: Default::default(),
        };
        let launch = Grok::default().launch(&opts).unwrap();
        assert_eq!(launch.program, PathBuf::from("/opt/grok/bin/grok"));
        assert_eq!(
            launch.args,
            [
                "--permission-mode",
                "default",
                "agent",
                "--reasoning-effort",
                "low",
                "stdio"
            ]
        );
    }

    #[test]
    fn recorded_resumed_turn_maps_to_the_canonical_stream() {
        let events = replay(RESUMED_TURN);

        let started = events
            .iter()
            .position(|event| matches!(event, AgentEvent::TurnStarted { .. }))
            .unwrap();
        assert!(
            !events[..started].iter().any(|event| matches!(
                event,
                AgentEvent::Delta { .. }
                    | AgentEvent::ItemStarted(_)
                    | AgentEvent::ItemUpdated(_)
                    | AgentEvent::ItemCompleted(_)
            )),
            "a resumed session replays no history: {:#?}",
            &events[..started]
        );

        assert_eq!(streamed(&events, "thought-1"), "Counting first.");
        assert!(matches!(completed(&events, "thought-1"),
            ItemContent::Reasoning { text } if text == "Counting first."));

        // Each shell update carries the whole output so far; the item shows
        // it, never the command's description or Grok's typed result.
        let shell = command_outputs(&events, "call_33_1");
        assert_eq!(
            shell.last(),
            Some(&("line1\nline2\nline3\n", Some(0))),
            "{shell:?}"
        );
        assert!(
            shell
                .windows(2)
                .all(|pair| pair[1].0.starts_with(pair[0].0) && pair[0].1.is_none()),
            "{shell:?}"
        );
        assert!(shell.iter().any(|(output, _)| *output == "line1\n"));
        assert_eq!(
            command_outputs(&events, "call_34_0").last(),
            Some(&("", Some(0)))
        );

        // The diff is the hunk Grok sent, placed at its `_meta` line.
        assert!(matches!(completed(&events, "call_35_0"),
            ItemContent::FileChange { changes, status: ItemStatus::Completed, .. }
                if changes.len() == 1 && changes[0].kind == FileChangeKind::Modify
                    && changes[0].path.ends_with("/in.txt")
                    && changes[0].diff.as_deref() == Some(
                        "--- a//private/tmp/grok-fixture/cwork/in.txt\n+++ b//private/tmp/grok-fixture/cwork/in.txt\n@@ -1,1 +1,1 @@\n-hello\n+bye\n")));
        assert!(matches!(completed(&events, "call_37_0"),
            ItemContent::ToolCall { name, output: Some(output), status: ItemStatus::Completed, .. }
                if name == "tcode_probe/echo_upper" && output == "HELLO MCP"));

        assert_eq!(
            streamed(&events, "msg-2"),
            "Counted, edited, asked and echoed."
        );
        assert!(matches!(completed(&events, "msg-2"),
            ItemContent::AssistantMessage { text, .. } if text == "Counted, edited, asked and echoed."));

        assert!(events.iter().any(|event| matches!(event,
            AgentEvent::TokenUsage(usage) if usage.used_tokens == Some(1234))));
        let completions: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TurnCompleted { status, usage, .. } => Some((*status, *usage)),
                _ => None,
            })
            .collect();
        assert_eq!(completions.len(), 1, "{completions:?}");
        let (status, usage) = completions[0];
        assert_eq!(status, TurnStatus::Completed);
        let usage = usage.expect("the turn reports its usage");
        assert_eq!(usage.freshness, ContextFreshness::Current);
        assert_eq!(usage.turn_processed_tokens, Some(7404));
        assert_eq!(usage.used_tokens, Some(1234));
    }

    /// Grok keys a question by its text and takes every answer as a list of
    /// the chosen labels.
    #[test]
    fn a_recorded_question_is_asked_and_answered_in_grok_s_shape() {
        let (params, answered) = exchange(RESUMED_TURN, ASK_USER_QUESTION);
        let asked = questions(&params).unwrap();
        let [question] = asked.as_slice() else {
            panic!("{asked:?}");
        };
        assert_eq!(question.id, "Which greeting should I use?");
        assert_eq!(question.question, "Which greeting should I use?");
        assert_eq!(
            question
                .options
                .iter()
                .map(|option| (option.label.as_str(), option.description.as_str()))
                .collect::<Vec<_>>(),
            [("Hello", "Friendly"), ("Hi", "Short")]
        );
        assert!(!question.multi_select);
        let mut answers = Map::new();
        answers.insert(question.id.clone(), json!("Hello"));
        assert_eq!(Value::Object(answer_lists(answers)), answered["answers"]);
    }

    /// Grok reports a background task that finished during a sent turn with
    /// a turn of its own, which no `session/prompt` answers. It starts while
    /// the sent turn's result is still on its way.
    #[test]
    fn a_turn_grok_starts_itself_runs_between_the_sent_turns() {
        let events = replay(AGENT_STARTED_TURN);

        let agent = "task-completed-00000000-0000-7000-8000-000000000083";
        let at = |wanted: &dyn Fn(&AgentEvent) -> bool| {
            events
                .iter()
                .position(wanted)
                .unwrap_or_else(|| panic!("{events:#?}"))
        };
        let lifecycle: Vec<(&str, &str, Option<u64>)> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TurnStarted { turn_id } => Some(("started", turn_id.as_str(), None)),
                AgentEvent::TurnCompleted { turn_id, usage, .. } => Some((
                    "completed",
                    turn_id.as_str(),
                    usage.and_then(|usage| usage.turn_processed_tokens),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            lifecycle,
            [
                ("started", "turn-1", None),
                ("completed", "turn-1", Some(3702)),
                ("started", agent, None),
                ("completed", agent, Some(1234)),
                ("started", "turn-2", None),
                ("completed", "turn-2", Some(1234)),
            ]
        );

        let reply = at(&|event| {
            matches!(event, AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text, .. }, ..
            }) if text == "The background task printed bgdone.")
        });
        let agent_started =
            at(&|event| matches!(event, AgentEvent::TurnStarted { turn_id } if turn_id == agent));
        let agent_completed = at(
            &|event| matches!(event, AgentEvent::TurnCompleted { turn_id, .. } if turn_id == agent),
        );
        assert!(agent_started < reply && reply < agent_completed);

        // The runtime must not take the process for idle before Grok's turn.
        let sent_completed = at(
            &|event| matches!(event, AgentEvent::TurnCompleted { turn_id, .. } if turn_id == "turn-1"),
        );
        let drained = at(&|event| matches!(event, AgentEvent::BackgroundTasksChanged { count: 0 }));
        assert!(sent_completed < drained, "{events:#?}");

        // The backgrounded command's call returns at once, and its card runs
        // in the background through its later output until the task ends.
        let backgrounded = at(&|event| {
            matches!(event, AgentEvent::ItemBackgrounded { item_id, parent_item_id: None }
                if item_id == "call_68_0")
        });
        let settled =
            at(&|event| matches!(event, AgentEvent::ItemCompleted(item) if item.id == "call_68_0"));
        assert!(backgrounded < settled, "{events:#?}");
        let background = events
            .iter()
            .take(settled)
            .filter(|event| {
                matches!(event, AgentEvent::ItemUpdated(item) | AgentEvent::ItemCompleted(item)
                    if item.id == "call_68_0")
            })
            .collect::<Vec<_>>();
        assert!(
            background.iter().all(|event| matches!(
                event,
                AgentEvent::ItemUpdated(ThreadItem {
                    content: ItemContent::CommandExecution {
                        status: ItemStatus::InProgress,
                        ..
                    },
                    ..
                })
            )),
            "{background:#?}"
        );
        assert!(matches!(completed_last(&events, "call_68_0"),
            ItemContent::CommandExecution { output, exit_code: Some(0), status: ItemStatus::Completed, .. }
                if output == "bgdone\n"));

        // Grok lists the task with the call that started it and when it
        // started, and lists none once it has ended.
        let lists: Vec<&Vec<crate::BackgroundTask>> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::BackgroundTaskList { tasks } => Some(tasks),
                _ => None,
            })
            .collect();
        assert!(
            lists
                .first()
                .is_some_and(|tasks| matches!(tasks.as_slice(), [task]
                if task.item_id.as_deref() == Some("call_68_0")
                    && task.name == "sleep 1; echo bgdone"
                    && task.kind == crate::BackgroundKind::Command
                    && task.started_reported
                    && task.started_at == 1_791_014_090_693)),
            "{lists:#?}"
        );
        assert!(
            lists.last().is_some_and(|tasks| tasks.is_empty()),
            "{lists:#?}"
        );
    }

    /// Grok's fork keeps the prompt it names, so a fork before prompt 3
    /// names prompt 2. The params are the ones grok-build's
    /// `xai-grok-shell/src/session/fork.rs` takes.
    #[test]
    fn a_fork_before_a_prompt_names_the_prompt_before_it() {
        let opts = SessionOptions {
            cwd: PathBuf::from("/home/user/project"),
            model: None,
            resume: None,
            fork: None,
            binary_path: None,
            option_selections: Vec::new(),
            mcp_servers: Vec::new(),
            launch_env: LaunchEnv::default(),
            extra_args: Vec::new(),
            acp: None,
            instructions: None,
            processes: Default::default(),
            clock: Default::default(),
        };
        let before = |checkpoint_id: &str| ForkPoint::Before {
            checkpoint_id: checkpoint_id.into(),
        };
        assert_eq!(
            fork_params("source-session", &before("3"), &opts).unwrap(),
            json!({"sourceSessionId": "source-session", "sourceCwd": "/home/user/project",
                "newCwd": "/home/user/project", "targetPromptIndex": 2})
        );
        assert_eq!(
            fork_params("source-session", &ForkPoint::Tail, &opts).unwrap(),
            json!({"sourceSessionId": "source-session", "sourceCwd": "/home/user/project",
                "newCwd": "/home/user/project"})
        );
        assert!(fork_params("source-session", &before("0"), &opts).is_err());
    }

    /// `_x.ai/rewind/execute` answers in the shapes of grok-build's
    /// `xai-grok-shell/src/extensions/rewind.rs` and `session/acp_types.rs`:
    /// a dry run lists the files a restore would change and those changed
    /// outside Grok, and the rewind returns the prompt and the files it
    /// restored.
    #[test]
    fn a_rewind_reads_grok_s_own_answers() {
        let preview = dry_run(&json!({"success": false, "target_prompt_index": 1, "mode": "files_only",
            "reverted_files": [], "clean_files": ["in.txt"],
            "conflicts": [{"path": "notes.md", "conflict_type": "modified_externally"}],
            "prompt_text": null, "error": "External modifications detected. Confirm to revert anyway."}))
        .unwrap();
        assert_eq!(
            serde_json::to_value(preview).unwrap(),
            json!({
                "files": {"files": [{"path": "in.txt"}, {"path": "notes.md"}]},
                "conflicts": [{"path": "notes.md", "kind": "modified_externally"}],
            })
        );
        let rewound = rewound(
            &json!({"success": true, "target_prompt_index": 1, "mode": "all",
                "reverted_files": ["in.txt", "notes.md"], "clean_files": [], "conflicts": [],
                "prompt_text": "Count, edit, ask and echo", "error": null}),
            "1",
            RewindMode::FilesAndConversation,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(rewound).unwrap(),
            json!({"type": "rewind_completed", "checkpoint_id": "1", "mode": "files_and_conversation",
                "prefill": "Count, edit, ask and echo", "later_history": "truncated",
                "files": {"files": [{"path": "in.txt"}, {"path": "notes.md"}]}})
        );
    }
}
