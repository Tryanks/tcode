//! `tcode_tasks`: shell commands Tcode runs for a thread whose harness has no
//! background commands of its own, so a long build, test run or dev server
//! neither holds the turn nor dies with a tool-call timeout, and its end wakes
//! the thread.
//!
//! The host authorizes and starts every command ([`BrokerRequest`]); the
//! tools read, wait on, move and stop what it started through the shared
//! [`TaskManager`].

pub mod manager;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProgressNotificationParam, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;

pub use manager::{
    Mover, Output, Phase, Spawn, TaskEvent, TaskInfo, TaskManager, Window, stop_every_task,
};

/// The MCP server name, and the tools' namespace.
pub const SERVER_NAME: &str = "tcode_tasks";

/// How long `task_run` waits before it moves the command to the background,
/// when the call does not say: under the 60 s most MCP clients give a call.
pub const DEFAULT_YIELD_S: u64 = 30;
/// The longest `task_run` waits, and `task_wait` waits, in one call.
pub const MAX_WAIT_S: u64 = 600;
/// How much output a result carries.
const RESULT_TAIL_LINES: usize = 200;
const RESULT_TAIL_BYTES: usize = 16 * 1024;
/// The largest window `task_read` returns.
const READ_MAX_BYTES: usize = 64 * 1024;
/// How often a waiting call tells a client that asked for progress that it
/// still works, which keeps clients that time out without it waiting.
const PROGRESS_EVERY: Duration = Duration::from_secs(15);
/// How often a running call looks for new output to stream.
const STREAM_EVERY: Duration = Duration::from_millis(200);
/// The most output one progress notification carries.
const STREAM_CHUNK: usize = 256 * 1024;
/// How much of the output a `tail` progress notification shows.
const STREAM_TAIL_BYTES: usize = 4 * 1024;
/// The most output a `delta` result carries after what was streamed.
const REST_MAX_BYTES: usize = 8 * 1024 * 1024;

/// What the progress notifications of a running `task_run` carry as their
/// `message`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputProgress {
    /// The end of the output so far, for a client that shows the latest
    /// message.
    #[default]
    Tail,
    /// Only the output new since the last notification; heartbeats carry an
    /// empty message. The result's `output_rest` holds what no notification
    /// carried.
    Delta,
}

/// The server's `instructions`, which the client reads at initialization and
/// which clients that defer tool definitions show the model up front.
pub const INSTRUCTIONS: &str = "tcode_tasks runs shell commands for this thread on the host. Use task_run instead of your own shell tool for anything that can take long (builds, test suites, installs, dev servers, watchers): it returns the output when the command exits, or moves the command to the background after about 30 s and returns what it printed so far, and Tcode then wakes this thread with a [control] message when the command exits. So never poll, sleep or re-run a command to see whether it finished; end your turn, or do other work, and the wake comes. Use task_start for a command you know runs on, such as a server. task_list, task_read, task_wait and task_stop work on this thread's tasks, which outlive restarts of your session.";

/// A command for the host to authorize and start.
pub struct BrokerRequest {
    pub session_id: String,
    pub command: String,
    /// As the model gave it; the host resolves it against the thread's
    /// working directory.
    pub cwd: Option<String>,
    /// A `task_run` call waits on it.
    pub foreground: bool,
    pub notify_on_exit: bool,
    /// The whole environment to run with; `None` for the host's own.
    pub env: Option<BTreeMap<String, String>>,
    pub timeout: Option<Duration>,
    /// The client's id for the tool call that runs it, which the user's
    /// "move to background" on that call names.
    pub call_id: Option<String>,
    pub reply: async_channel::Sender<Result<TaskInfo, String>>,
}

pub type Broker = mcp_host::Broker<BrokerRequest>;
pub type Service = StreamableHttpService<TaskTools, LocalSessionManager>;
pub type TokenRegistry = mcp_host::TokenRegistry<Service>;
/// The manager the tools act through, set once the host has opened it.
pub type ManagerSlot = Arc<OnceLock<TaskManager>>;

pub struct TasksMcpServer {
    pub url: String,
    pub tokens: TokenRegistry,
    pub requests: async_channel::Receiver<BrokerRequest>,
    pub manager: ManagerSlot,
}

pub fn start(host: &mut mcp_host::Host) -> TasksMcpServer {
    let (sender, requests) = async_channel::unbounded();
    let broker = Broker::new(
        sender,
        mcp_host::BrokerErrors {
            unavailable: "Tcode's task host is unavailable",
            dropped: "Tcode's task host dropped the request",
        },
    );
    let manager = ManagerSlot::default();
    let shared = manager.clone();
    let tokens = TokenRegistry::new(move |session_id| {
        let broker = broker.clone();
        let manager = shared.clone();
        StreamableHttpService::new(
            move || {
                Ok(TaskTools::new(
                    broker.clone(),
                    manager.clone(),
                    session_id.clone(),
                ))
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        )
    });
    host.mount(mcp_host::route("/tasks", &tokens));
    TasksMcpServer {
        url: host.url("/tasks"),
        tokens,
        requests,
        manager,
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunParams {
    /// The shell command line, run by bash -c (cmd /C on Windows) with no
    /// stdin, so nothing may wait for input. stdout and stderr are merged.
    pub command: String,
    /// Working directory: absolute, or relative to the thread's working
    /// directory, which is the default.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Seconds to wait for the exit before the command moves to the
    /// background and the call returns (default 30, at most 600). Keep it
    /// under your MCP client's tool-call timeout, usually 60 s: a call the
    /// client gives up on is cancelled, and a cancelled call kills the
    /// command.
    #[serde(default)]
    pub yield_after_s: Option<u64>,
    /// Seconds after which the command is stopped, in the foreground or the
    /// background (default: never). It ends with timed_out true.
    #[serde(default)]
    pub timeout_s: Option<f64>,
    /// The complete environment to run with, instead of the host's.
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    /// A file holding that environment as one JSON object of strings, read
    /// when the command starts, for a client that keeps it out of the call's
    /// record.
    #[serde(default)]
    pub env_file: Option<String>,
    /// Your own id for this tool call, so Tcode's "move to background" on
    /// the call's card reaches this command.
    #[serde(default)]
    pub call_id: Option<String>,
    /// What progress notifications carry while the command runs (default
    /// tail: the end of the output so far; delta: only the new output).
    #[serde(default)]
    pub output_progress: Option<OutputProgress>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartParams {
    /// The shell command line, run as task_run runs it.
    pub command: String,
    /// Working directory: absolute, or relative to the thread's working
    /// directory, which is the default.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Wake this thread with a [control] message when the command exits
    /// (default true).
    #[serde(default)]
    pub notify_on_exit: Option<bool>,
    /// Seconds after which the command is stopped (default: never).
    #[serde(default)]
    pub timeout_s: Option<f64>,
    /// The complete environment to run with, instead of the host's.
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskIdParams {
    /// The task's id, such as "t3", from task_run, task_start or task_list.
    pub task_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    /// The task's id.
    pub task_id: String,
    /// Read from this byte offset (a previous result's `end` continues it).
    /// Without it, the last `tail_lines` lines are read.
    #[serde(default)]
    pub offset: Option<u64>,
    /// How many of the last lines to read when no offset is given (default
    /// 200).
    #[serde(default)]
    pub tail_lines: Option<usize>,
    /// The most bytes to return (default and maximum 65536).
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitParams {
    /// The task's id.
    pub task_id: String,
    /// Seconds to wait for the exit (default 30, at most 600); keep it under
    /// your MCP client's tool-call timeout.
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

/// How a foreground `task_run` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Followed {
    Exited(TaskInfo),
    Backgrounded(TaskInfo),
    /// The call was cancelled, and the command stopped.
    Cancelled(TaskInfo),
}

/// Stops a foreground task whose call goes away without an answer.
struct StopUnanswered<'a> {
    manager: &'a TaskManager,
    session_id: &'a str,
    task_id: &'a str,
    armed: bool,
}

impl Drop for StopUnanswered<'_> {
    fn drop(&mut self) {
        if self.armed
            && self
                .manager
                .get(self.session_id, self.task_id)
                .is_ok_and(|task| task.foreground && task.phase.running())
        {
            let _ = self.manager.stop(self.session_id, self.task_id);
        }
    }
}

/// Wait on the foreground task `task_id` until it exits, moves to the
/// background (moved by the user or the model, or itself after
/// `yield_after`), or `cancelled` completes, which stops it. A call dropped
/// unanswered stops it too.
pub async fn follow(
    manager: &TaskManager,
    session_id: &str,
    task_id: &str,
    yield_after: Duration,
    cancelled: impl Future<Output = ()>,
) -> Result<Followed, String> {
    let mut state = manager.watch(session_id, task_id)?;
    let mut guard = StopUnanswered {
        manager,
        session_id,
        task_id,
        armed: true,
    };
    let deadline = tokio::time::sleep(yield_after);
    tokio::pin!(deadline, cancelled);
    let followed = loop {
        let now = state.borrow_and_update().clone();
        if !now.phase.running() {
            manager.mark_reported(session_id, task_id);
            break Followed::Exited(manager.get(session_id, task_id)?);
        }
        if !now.foreground {
            break Followed::Backgrounded(manager.get(session_id, task_id)?);
        }
        tokio::select! {
            changed = state.changed() => {
                if changed.is_err() {
                    break Followed::Exited(manager.get(session_id, task_id)?);
                }
            }
            () = &mut deadline => {
                match manager.move_to_background(session_id, task_id, Mover::Yield) {
                    Ok(task) => break Followed::Backgrounded(task),
                    // It ended meanwhile: the next look reports the exit.
                    Err(_) => continue,
                }
            }
            () = &mut cancelled => {
                manager.stop(session_id, task_id)?;
                break Followed::Cancelled(manager.get(session_id, task_id)?);
            }
        }
    };
    guard.armed = false;
    Ok(followed)
}

/// Wait up to `timeout` for the task to end; its standing then.
pub async fn wait(
    manager: &TaskManager,
    session_id: &str,
    task_id: &str,
    timeout: Duration,
) -> Result<TaskInfo, String> {
    let mut state = manager.watch(session_id, task_id)?;
    let ended = tokio::time::timeout(timeout, state.wait_for(|now| !now.phase.running())).await;
    if matches!(ended, Ok(Ok(_))) {
        manager.mark_reported(session_id, task_id);
    }
    manager.get(session_id, task_id)
}

/// A command a call asks the host to start.
struct Launch {
    command: String,
    cwd: Option<String>,
    foreground: bool,
    notify_on_exit: bool,
    env: Option<BTreeMap<String, String>>,
    env_file: Option<String>,
    timeout_s: Option<f64>,
    call_id: Option<String>,
}

#[derive(Clone)]
pub struct TaskTools {
    broker: Broker,
    manager: ManagerSlot,
    session_id: String,
    tool_router: ToolRouter<Self>,
}

fn text_result(value: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

fn error_result(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn not_ready() -> CallToolResult {
    error_result("Tcode's task host is not ready")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// The task as a result reports it, with the tail of its output.
fn report(
    manager: &TaskManager,
    session_id: &str,
    task: &TaskInfo,
    status: &str,
) -> serde_json::Value {
    let mut value = serde_json::to_value(task).unwrap_or_default();
    let object = value
        .as_object_mut()
        .expect("a task serializes as an object");
    object.insert("status".into(), json!(status));
    object.insert(
        "duration_s".into(),
        json!(task.duration_ms(now_ms()) as f64 / 1000.0),
    );
    if let Ok(output) = manager.read(
        session_id,
        &task.task_id,
        Window::Tail {
            lines: RESULT_TAIL_LINES,
            max_bytes: RESULT_TAIL_BYTES,
        },
    ) {
        if output.start > 0 {
            object.insert(
                "output_note".into(),
                json!(format!(
                    "the last {} of {} bytes; task_read with an offset reads the rest",
                    output.end - output.start,
                    output.total_bytes
                )),
            );
        }
        object.insert("output".into(), json!(output.text));
    }
    value
}

/// Keep a client that asked for progress from timing the call out while
/// `work` runs.
async fn with_progress<T>(
    context: &RequestContext<RoleServer>,
    message: &str,
    work: impl Future<Output = T>,
) -> T {
    let token = context.meta.get_progress_token();
    let peer = context.peer.clone();
    let message = message.to_owned();
    let progress = async move {
        let Some(token) = token else {
            return std::future::pending::<()>().await;
        };
        let mut ticks = 0u32;
        loop {
            tokio::time::sleep(PROGRESS_EVERY).await;
            ticks += 1;
            let note = ProgressNotificationParam::new(token.clone(), f64::from(ticks))
                .with_message(message.clone());
            let _ = peer.notify_progress(note).await;
        }
    };
    tokio::select! {
        result = work => result,
        () = progress => unreachable!("progress never ends"),
    }
}

/// The longest prefix of `bytes` that is whole UTF-8, as text, and its
/// length: a character cut at the end waits for the next read. Bytes that
/// never make UTF-8 are passed on as replacement characters.
fn utf8_prefix(bytes: &[u8]) -> (String, usize) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), bytes.len()),
        Err(error) if error.error_len().is_none() => {
            let valid = error.valid_up_to();
            (String::from_utf8_lossy(&bytes[..valid]).into_owned(), valid)
        }
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), bytes.len()),
    }
}

/// Stream the task's output to a client that asked for progress until
/// `done`, as `mode` says, beating at least every [`PROGRESS_EVERY`]. The
/// offset of the output the notifications carried.
async fn stream_output(
    context: &RequestContext<RoleServer>,
    manager: &TaskManager,
    session_id: &str,
    task_id: &str,
    mode: OutputProgress,
    mut done: tokio::sync::watch::Receiver<bool>,
) -> u64 {
    let Some(token) = context.meta.get_progress_token() else {
        let _ = done.wait_for(|done| *done).await;
        return 0;
    };
    let mut offset = 0u64;
    let mut seen = 0u64;
    let mut ticks = 0u32;
    let mut beat = tokio::time::Instant::now();
    loop {
        // Only the waits are given up at the end, never a notification half
        // sent, so what the result adds starts where the last one ended.
        tokio::select! {
            () = tokio::time::sleep(STREAM_EVERY) => {}
            _ = done.wait_for(|done| *done) => break,
        }
        let message = match mode {
            OutputProgress::Delta => manager
                .read_bytes(session_id, task_id, offset, STREAM_CHUNK)
                .map(|bytes| utf8_prefix(&bytes))
                .unwrap_or_default(),
            OutputProgress::Tail => {
                let total = manager
                    .get(session_id, task_id)
                    .map_or(seen, |task| task.output_bytes);
                if total == seen {
                    (String::new(), 0)
                } else {
                    seen = total;
                    let tail = manager
                        .read(
                            session_id,
                            task_id,
                            Window::Tail {
                                lines: RESULT_TAIL_LINES,
                                max_bytes: STREAM_TAIL_BYTES,
                            },
                        )
                        .map(|output| output.text)
                        .unwrap_or_default();
                    (tail, 1)
                }
            }
        };
        let (text, advanced) = message;
        if advanced == 0 && beat.elapsed() < PROGRESS_EVERY {
            continue;
        }
        ticks += 1;
        let note =
            ProgressNotificationParam::new(token.clone(), f64::from(ticks)).with_message(text);
        beat = tokio::time::Instant::now();
        if context.peer.notify_progress(note).await.is_ok() && mode == OutputProgress::Delta {
            offset += advanced as u64;
        }
    }
    offset
}

#[tool_router]
impl TaskTools {
    fn new(broker: Broker, manager: ManagerSlot, session_id: String) -> Self {
        Self {
            broker,
            manager,
            session_id,
            tool_router: Self::tool_router(),
        }
    }

    fn manager(&self) -> Option<&TaskManager> {
        self.manager.get()
    }

    async fn launch(&self, launch: Launch) -> Result<TaskInfo, String> {
        if launch.command.trim().is_empty() {
            return Err("the command is empty".into());
        }
        let env = match launch.env_file {
            None => launch.env,
            Some(path) => {
                let read = std::fs::read(&path)
                    .map_err(|error| format!("could not read env_file {path}: {error}"))?;
                Some(serde_json::from_slice(&read).map_err(|error| {
                    format!("env_file {path} is not a JSON object of strings: {error}")
                })?)
            }
        };
        let timeout = match launch.timeout_s {
            None => None,
            Some(secs) if secs.is_finite() && secs > 0.0 => Some(Duration::from_secs_f64(secs)),
            Some(_) => return Err("timeout_s must be a positive number of seconds".into()),
        };
        self.broker
            .invoke(|reply| BrokerRequest {
                session_id: self.session_id.clone(),
                command: launch.command,
                cwd: launch.cwd,
                foreground: launch.foreground,
                notify_on_exit: launch.notify_on_exit,
                env,
                timeout,
                call_id: launch.call_id,
                reply,
            })
            .await
    }

    #[tool(
        description = "Run a shell command on the host for this thread and wait for it, for anything that may take more than a few seconds (builds, tests, installs, dev servers). Returns {status:\"exited\", task_id, exit_code (or signal), duration_s, output} when it exits within yield_after_s (default 30). Otherwise the command keeps running in the background and the call returns {status:\"backgrounded\", task_id, moved_by, output (printed so far)}; Tcode then wakes this thread with a [control] message giving the exit code and the end of the output when it exits, so do not poll or sleep for it: end your turn or do other work. The user can also move the command to the background from Tcode, and task_background does it from another call. output is the last 200 lines (at most 16 KB); task_read reads the rest. While it runs, progress notifications carry its output (output_progress). timeout_s stops the command wherever it runs (the result then has timed_out true); it is not yield_after_s, which only moves it to the background. If this call is cancelled (the user interrupts the turn), the command is killed. Commands run without stdin, with stdout and stderr merged, and may need the user's approval in Tcode first; a declined command returns an error."
    )]
    async fn task_run(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<RunParams>,
    ) -> CallToolResult {
        let Some(manager) = self.manager() else {
            return not_ready();
        };
        let yield_after = Duration::from_secs(
            params
                .yield_after_s
                .unwrap_or(DEFAULT_YIELD_S)
                .clamp(1, MAX_WAIT_S),
        );
        let mode = params.output_progress.unwrap_or_default();
        let beat = match mode {
            OutputProgress::Tail => "the command is still running",
            OutputProgress::Delta => "",
        };
        let ct = context.ct.clone();
        let launch = Launch {
            command: params.command,
            cwd: params.cwd,
            foreground: true,
            notify_on_exit: false,
            env: params.env,
            env_file: params.env_file,
            timeout_s: params.timeout_s,
            call_id: params.call_id,
        };
        let launched = with_progress(&context, beat, async {
            tokio::select! {
                task = self.launch(launch) => task,
                () = ct.cancelled() => Err("cancelled before the command started".to_owned()),
            }
        })
        .await;
        let task = match launched {
            Ok(task) => task,
            Err(error) => return error_result(error),
        };
        let (done, streaming) = tokio::sync::watch::channel(false);
        let (followed, streamed) = tokio::join!(
            async {
                let followed = follow(
                    manager,
                    &self.session_id,
                    &task.task_id,
                    yield_after,
                    ct.cancelled(),
                )
                .await;
                let _ = done.send(true);
                followed
            },
            stream_output(
                &context,
                manager,
                &self.session_id,
                &task.task_id,
                mode,
                streaming,
            ),
        );
        let rest = |mut value: serde_json::Value| {
            if mode == OutputProgress::Delta {
                let bytes = manager
                    .read_bytes(&self.session_id, &task.task_id, streamed, REST_MAX_BYTES)
                    .unwrap_or_default();
                value["output_rest"] = json!(String::from_utf8_lossy(&bytes));
            }
            value
        };
        match followed {
            Ok(Followed::Exited(task)) => {
                text_result(rest(report(manager, &self.session_id, &task, "exited")))
            }
            Ok(Followed::Backgrounded(task)) => {
                let mut value = rest(report(manager, &self.session_id, &task, "backgrounded"));
                value["note"] = json!(
                    "The command keeps running. Tcode wakes this thread with a [control] message when it exits; use task_read, task_wait or task_stop with its task_id meanwhile."
                );
                text_result(value)
            }
            Ok(Followed::Cancelled(task)) => error_result(format!(
                "the call was cancelled and task {} was stopped",
                task.task_id
            )),
            Err(error) => error_result(error),
        }
    }

    #[tool(
        description = "Start a shell command on the host for this thread in the background and return {task_id} at once. Use it for commands that run on (dev servers, watchers) or that you know take long. With notify_on_exit (default true) Tcode wakes this thread with a [control] message giving the exit code and the end of the output when it exits. Read its output with task_read, wait on it with task_wait, end it with task_stop. Commands run without stdin, with stdout and stderr merged, and may need the user's approval in Tcode first."
    )]
    async fn task_start(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<StartParams>,
    ) -> CallToolResult {
        let launched = with_progress(
            &context,
            "waiting for the command to start",
            self.launch(Launch {
                command: params.command,
                cwd: params.cwd,
                foreground: false,
                notify_on_exit: params.notify_on_exit.unwrap_or(true),
                env: params.env,
                env_file: None,
                timeout_s: params.timeout_s,
                call_id: None,
            }),
        )
        .await;
        match launched {
            Ok(task) => {
                let mut value = serde_json::to_value(&task).unwrap_or_default();
                value["status"] = json!("started");
                text_result(value)
            }
            Err(error) => error_result(error),
        }
    }

    #[tool(
        description = "List this thread's tasks, oldest first: task_id, command, cwd, started_at (unix ms), state (running, exited with exit_code or signal and ended_at, or lost: Tcode restarted while it ran and no longer knows of it), foreground (a task_run call still waits on it), notify_on_exit and output_bytes."
    )]
    async fn task_list(&self) -> CallToolResult {
        match self.manager() {
            Some(manager) => text_result(json!({ "tasks": manager.list(&self.session_id) })),
            None => not_ready(),
        }
    }

    #[tool(
        description = "Read a task's output (stdout and stderr merged): by default its last tail_lines lines (200); with offset, the bytes from there on. Returns {text, start, end, total_bytes}; pass end as the next offset to continue. At most max_bytes (65536) per call."
    )]
    async fn task_read(&self, Parameters(params): Parameters<ReadParams>) -> CallToolResult {
        let Some(manager) = self.manager() else {
            return not_ready();
        };
        let max_bytes = params
            .max_bytes
            .unwrap_or(READ_MAX_BYTES)
            .clamp(1, READ_MAX_BYTES);
        let window = match params.offset {
            Some(offset) => Window::From { offset, max_bytes },
            None => Window::Tail {
                lines: params.tail_lines.unwrap_or(RESULT_TAIL_LINES).max(1),
                max_bytes,
            },
        };
        match manager.read(&self.session_id, &params.task_id, window) {
            Ok(output) => text_result(serde_json::to_value(output).unwrap_or_default()),
            Err(error) => error_result(error),
        }
    }

    #[tool(
        description = "Wait up to timeout_s (default 30, at most 600; keep it under your client's tool-call timeout) for a task to exit. Returns the task with status exited (and the end of its output) or running when the time ran out. A task that exits while you wait on it here wakes nobody. Prefer ending your turn to waiting: a background task's exit wakes this thread."
    )]
    async fn task_wait(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<WaitParams>,
    ) -> CallToolResult {
        let Some(manager) = self.manager() else {
            return not_ready();
        };
        let timeout = Duration::from_secs(
            params
                .timeout_s
                .unwrap_or(DEFAULT_YIELD_S)
                .clamp(1, MAX_WAIT_S),
        );
        let ct = context.ct.clone();
        let waited = with_progress(&context, "waiting for the command to exit", async {
            tokio::select! {
                task = wait(manager, &self.session_id, &params.task_id, timeout) => task,
                () = ct.cancelled() => Err("cancelled".to_owned()),
            }
        })
        .await;
        match waited {
            Ok(task) => {
                let status = if task.phase.running() {
                    "running"
                } else {
                    "exited"
                };
                text_result(report(manager, &self.session_id, &task, status))
            }
            Err(error) => error_result(error),
        }
    }

    #[tool(
        description = "Move a task that a task_run call is waiting on to the background: that call returns at once with status backgrounded, the command keeps running, and its exit wakes this thread."
    )]
    async fn task_background(
        &self,
        Parameters(params): Parameters<TaskIdParams>,
    ) -> CallToolResult {
        let Some(manager) = self.manager() else {
            return not_ready();
        };
        match manager.move_to_background(&self.session_id, &params.task_id, Mover::Agent) {
            Ok(task) => text_result(serde_json::to_value(task).unwrap_or_default()),
            Err(error) => error_result(error),
        }
    }

    #[tool(
        description = "Stop a running task: SIGTERM to its process group, SIGKILL 3 s later to whatever is left. Returns the task; its exit is reported as for any other (a stopped task with notify_on_exit still wakes this thread)."
    )]
    async fn task_stop(&self, Parameters(params): Parameters<TaskIdParams>) -> CallToolResult {
        let Some(manager) = self.manager() else {
            return not_ready();
        };
        match manager.stop(&self.session_id, &params.task_id) {
            Ok(task) => text_result(serde_json::to_value(task).unwrap_or_default()),
            Err(error) => error_result(error),
        }
    }
}

/// Every tool's name and the description its model reads, in the order they
/// are listed.
pub fn tool_descriptions() -> Vec<(String, String)> {
    TaskTools::tool_router()
        .list_all()
        .into_iter()
        .map(|tool| {
            (
                tool.name.to_string(),
                tool.description.as_deref().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TaskTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests;
