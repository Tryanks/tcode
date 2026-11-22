//! Subagent discovery and transcript tailing for Google Antigravity CLI (`agy`).
//!
//! Over ACP `invoke_subagent` arrives as an ordinary `other` tool call whose
//! immediate `completed` update only acknowledges the asynchronous spawn. The
//! spawned child's state and trajectory live under Antigravity's state
//! directory (agy 1.5.14):
//!
//! - `brain/<parent>/.system_generated/subagents/<child>.json` — descriptor
//!   (`typeName`, `role`), `state` (`SUBAGENT_STATE_ALIVE` /
//!   `SUBAGENT_STATE_KILLED`), and `spawnStepIndex`.
//! - `brain/<child>/.system_generated/logs/transcript.jsonl` — step-by-step
//!   JSONL with `model`, `thinking`, `tool_calls`, `GENERIC` tool outputs, and
//!   the child's final `PLANNER_RESPONSE`.
//! - `conversations/<child>.db` — reports when a child turn ends on an error
//!   step ([`super::record`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::v1::schema as acp;
use serde::Deserialize;
use serde_json::Value;
use smol::channel::{Receiver, Sender};
use smol::future;

use super::record;
use crate::acp_session::{State, TurnEnd};
use crate::{AgentEvent, ItemContent, ItemStatus, ThreadItem, ToolFamily, TurnStatus};

const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Metadata extracted from a parent `invoke_subagent` ACP tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SpawnedSubagent {
    pub(super) tool_call_id: String,
    pub(super) agent_type: String,
    pub(super) description: String,
    pub(super) prompt: Option<String>,
    pub(super) model: Option<String>,
}

/// Control messages sent from `Antigravity` callbacks to its background
/// watcher task.
pub(super) enum WatcherCommand {
    /// A new `invoke_subagent` call was observed on the ACP wire.
    Spawned(SpawnedSubagent),
    /// An `invoke_subagent` call failed before matching a child file.
    SpawnFailed(String),
    /// A parent `send_message` call targeted `recipient` (possibly a subagent).
    MessageSent {
        recipient: String,
        message: Option<String>,
    },
    /// Sync `proactive.seen` to the latest SQLite step index (e.g. after an
    /// interrupted proactive turn so any late step does not bleed into the
    /// next turn).
    SyncSeen,
    /// Wake the watcher immediately (e.g. `manage_subagents` killed a child or
    /// a proactive `usage_update` arrived).
    Wake,
    /// The session is closing: settle any still-running subagents as
    /// interrupted, flush pending output, and stop.
    Close,
}

/// Shared proactive-turn coordination between `Antigravity` callbacks
/// and the session's watcher. Acquire `State` before `ProactiveTracker`
/// whenever both locks are taken.
#[derive(Default)]
pub(super) struct ProactiveTracker {
    pub(super) active: Option<ActiveProactive>,
    pub(super) seen: Option<i64>,
}

pub(super) struct ActiveProactive {
    pub(super) turn_id: String,
    pub(super) since: Option<i64>,
    /// True once a `usage_update` arrived after the latest tool call, so the
    /// watcher may check for the terminal transcript step on disk.
    pub(super) usage_seen: bool,
}

/// Parse an ACP `tool_call` notification as an Antigravity `invoke_subagent`
/// spawn.
pub(super) fn parse_spawn(call: &acp::ToolCall) -> Option<SpawnedSubagent> {
    if call.title != "Run invoke_subagent" {
        return None;
    }
    let input = call.raw_input.as_ref()?.as_object()?;
    let agent_type = input
        .get("TypeName")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())?
        .to_owned();
    let prompt = input
        .get("Prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    let role = input
        .get("Role")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    let description = role
        .or_else(|| {
            prompt
                .as_deref()
                .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
                .map(|line| line.trim().to_owned())
        })
        .unwrap_or_else(|| agent_type.clone());
    let model = input
        .get("Model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && *value != "inherit")
        .map(str::to_owned);
    Some(SpawnedSubagent {
        tool_call_id: call.tool_call_id.0.to_string(),
        agent_type,
        description,
        prompt,
        model,
    })
}

/// Extract `(recipient, message)` from a parent `send_message` ACP tool call.
pub(super) fn parse_send_message(call: &acp::ToolCall) -> Option<(String, Option<String>)> {
    if call.title != "Run send_message" {
        return None;
    }
    let input = call.raw_input.as_ref()?.as_object()?;
    let recipient = input
        .get("Recipient")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())?
        .to_owned();
    let message = input
        .get("Message")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    Some((recipient, message))
}

/// Whether `call` is a parent `manage_subagents` call (e.g. kill).
pub(super) fn is_manage_subagents(call: &acp::ToolCall) -> bool {
    call.title == "Run manage_subagents"
}

/// Initial canonical `Subagent` item emitted when `invoke_subagent` starts,
/// plus the child's opening `UserMessage` item carrying the spawn prompt.
pub(super) fn spawn_events(spawn: &SpawnedSubagent) -> Vec<AgentEvent> {
    let mut events = vec![AgentEvent::ItemStarted(ThreadItem {
        id: spawn.tool_call_id.clone(),
        parent_item_id: None,
        content: ItemContent::Subagent {
            agent_type: spawn.agent_type.clone(),
            description: spawn.description.clone(),
            status: ItemStatus::InProgress,
            summary: None,
            model: spawn.model.clone(),
            effort: None,
        },
    })];
    if let Some(prompt) = &spawn.prompt {
        events.push(AgentEvent::ItemCompleted(ThreadItem {
            id: format!("{}:prompt", spawn.tool_call_id),
            parent_item_id: Some(spawn.tool_call_id.clone()),
            content: ItemContent::UserMessage {
                text: prompt.clone(),
                attachments: Vec::new(),
            },
        }));
    }
    events
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentFile {
    conversation_id: String,
    #[serde(default)]
    subagent_descriptor: SubagentDescriptor,
    #[serde(default)]
    state: String,
    #[serde(default)]
    spawn_step_index: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentDescriptor {
    #[serde(default)]
    type_name: String,
    #[serde(default)]
    role: String,
}

#[derive(Debug, Deserialize)]
struct ChildStep {
    step_index: i64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChildToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ChildToolCall {
    name: String,
    #[serde(default)]
    args: Value,
}

struct PendingChildTool {
    id: String,
    name: String,
    input: Value,
}

impl PendingChildTool {
    fn content(&self, status: ItemStatus, output: Option<String>) -> ItemContent {
        if self.name == "run_command" {
            let command = self
                .input
                .get("CommandLine")
                .and_then(Value::as_str)
                .unwrap_or("run_command")
                .to_owned();
            return ItemContent::CommandExecution {
                command,
                input: Vec::new(),
                output: output.unwrap_or_default(),
                exit_code: None,
                status,
            };
        }
        let family = if self.name == "read_file" {
            self.input.get("Path").and_then(Value::as_str).map(|path| {
                Box::new(ToolFamily::Read {
                    path: path.to_owned(),
                    first_line: self.input.get("StartLine").and_then(Value::as_u64),
                    note: None,
                })
            })
        } else {
            None
        };
        ItemContent::ToolCall {
            name: self.name.clone(),
            input: self.input.clone(),
            output,
            status,
            image_reads: Vec::new(),
            family,
        }
    }
}

/// Incremental JSONL tailer and canonical mapper for one Antigravity subagent.
pub(super) struct ChildTailer {
    parent_item_id: String,
    child_id: String,
    transcript_path: PathBuf,
    db_path: PathBuf,
    offset: u64,
    pending_tools: VecDeque<PendingChildTool>,
    model: Option<String>,
    last_reported: Option<String>,
    last_assistant: Option<String>,
    last_step_index: i64,
    last_error_step: Option<i64>,
    turn_done: bool,
}

impl ChildTailer {
    pub(super) fn new(
        parent_item_id: String,
        child_id: String,
        state_dir: &Path,
        initial_model: Option<String>,
    ) -> Self {
        Self {
            parent_item_id,
            transcript_path: record::transcript_path(state_dir, &child_id),
            db_path: record::path(state_dir, &child_id),
            child_id,
            offset: 0,
            pending_tools: VecDeque::new(),
            model: initial_model,
            last_reported: None,
            last_assistant: None,
            last_step_index: -1,
            last_error_step: None,
            turn_done: false,
        }
    }

    /// Read any newly appended complete lines from the child's `transcript.jsonl`.
    pub(super) fn poll(&mut self) -> Vec<AgentEvent> {
        let Ok(mut file) = File::open(&self.transcript_path) else {
            return Vec::new();
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut reader = BufReader::new(file);
        let mut events = Vec::new();
        loop {
            let mut line = String::new();
            let Ok(read) = reader.read_line(&mut line) else {
                break;
            };
            if read == 0 {
                break;
            }
            if !line.ends_with('\n') {
                // Partial trailing write; retry on the next poll once flushed.
                break;
            }
            self.offset += read as u64;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(step) = serde_json::from_str::<ChildStep>(trimmed) {
                events.extend(self.apply_step(step));
            }
        }
        events
    }

    fn apply_step(&mut self, step: ChildStep) -> Vec<AgentEvent> {
        if step.step_index > self.last_step_index {
            self.last_step_index = step.step_index;
        }
        let mut events = Vec::new();
        match step.kind.as_str() {
            "PLANNER_RESPONSE" => {
                if let Some(model) = step.model.filter(|m| !m.trim().is_empty()) {
                    self.model = Some(model);
                }
                if let Some(thinking) = step.thinking.filter(|t| !t.trim().is_empty()) {
                    events.push(AgentEvent::ItemCompleted(self.child_item(
                        format!("{}:s{}:thought", self.child_id, step.step_index),
                        ItemContent::Reasoning { text: thinking },
                    )));
                }
                let tool_calls = step.tool_calls.unwrap_or_default();
                let has_new_tools = !tool_calls.is_empty();
                if has_new_tools {
                    self.turn_done = false;
                    for (index, call) in tool_calls.into_iter().enumerate() {
                        let input = decode_transcript_args(&call.args);
                        if call.name == "send_message"
                            && let Some(msg) = input
                                .get("Message")
                                .and_then(Value::as_str)
                                .filter(|m| !m.trim().is_empty())
                        {
                            self.last_reported = Some(msg.to_owned());
                        }
                        let tool = PendingChildTool {
                            id: format!("{}:s{}:t{}", self.child_id, step.step_index, index),
                            name: call.name,
                            input,
                        };
                        let content = tool.content(ItemStatus::InProgress, None);
                        let id = tool.id.clone();
                        self.pending_tools.push_back(tool);
                        events.push(AgentEvent::ItemStarted(self.child_item(id, content)));
                    }
                }
                if let Some(text) = step.content.filter(|c| !c.trim().is_empty()) {
                    self.last_assistant = Some(text.clone());
                    events.push(AgentEvent::ItemCompleted(self.child_item(
                        format!("{}:s{}:msg", self.child_id, step.step_index),
                        ItemContent::AssistantMessage {
                            text,
                            summarized: false,
                        },
                    )));
                }
                if step.status == "DONE" && !has_new_tools {
                    while let Some(tool) = self.pending_tools.pop_front() {
                        let content = tool.content(ItemStatus::Completed, None);
                        events.push(AgentEvent::ItemCompleted(self.child_item(tool.id, content)));
                    }
                    self.turn_done = true;
                }
            }
            "GENERIC" => {
                if let Some(tool) = self.pending_tools.pop_front() {
                    let output = step.content.filter(|c| !c.trim().is_empty());
                    let content = tool.content(ItemStatus::Completed, output);
                    events.push(AgentEvent::ItemCompleted(self.child_item(tool.id, content)));
                }
            }
            _ => {}
        }
        events
    }

    fn child_item(&self, id: String, content: ItemContent) -> ThreadItem {
        ThreadItem {
            id,
            parent_item_id: Some(self.parent_item_id.clone()),
            content,
        }
    }

    fn summary(&self) -> Option<String> {
        self.last_reported
            .clone()
            .or_else(|| self.last_assistant.clone())
    }

    fn failed_in_db(&mut self) -> bool {
        let Some(last) = record::last_step(&self.db_path) else {
            return false;
        };
        if last.error
            && last.index >= self.last_step_index
            && self.last_error_step != Some(last.index)
        {
            self.last_error_step = Some(last.index);
            true
        } else {
            false
        }
    }
}

/// `agy` 1.5.14 serializes each tool argument in `transcript.jsonl` as a
/// JSON-encoded string (e.g. `{"Path": "\"/repo/a.txt\""}`, `{"DurationSeconds": "10"}`).
/// Unwrap JSON-encoded strings back into structured values for display.
fn decode_transcript_args(raw: &Value) -> Value {
    match raw {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let decoded = match value.as_str() {
                        Some(text) => {
                            serde_json::from_str::<Value>(text).unwrap_or_else(|_| value.clone())
                        }
                        None => value.clone(),
                    };
                    (key.clone(), decoded)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

struct TrackedSubagent {
    tool_call_id: String,
    agent_type: String,
    description: String,
    status: ItemStatus,
    model: Option<String>,
    tailer: ChildTailer,
    followup_seq: u64,
}

impl TrackedSubagent {
    fn snapshot(&self, status: ItemStatus, summary: Option<String>) -> ThreadItem {
        ThreadItem {
            id: self.tool_call_id.clone(),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: self.agent_type.clone(),
                description: self.description.clone(),
                status,
                summary,
                model: self.tailer.model.clone().or_else(|| self.model.clone()),
                effort: None,
            },
        }
    }
}

/// Stateful coordinator driven by [`run_watcher`].
pub(super) struct WatcherState {
    state_dir: PathBuf,
    parent_session_id: String,
    /// Child conversation ids already present on disk when the session started,
    /// so a resumed session does not pair historical subagents with new spawns.
    ignored_children: HashSet<String>,
    /// Parent `invoke_subagent` calls waiting for their `<child>.json` to appear.
    unmatched_spawns: VecDeque<SpawnedSubagent>,
    /// Active or completed tracked subagents keyed by child conversation id.
    children: HashMap<String, TrackedSubagent>,
}

impl WatcherState {
    pub(super) fn new(state_dir: PathBuf, parent_session_id: String) -> Self {
        let ignored_children = read_subagent_files(&state_dir, &parent_session_id)
            .into_iter()
            .map(|file| file.conversation_id)
            .collect();
        Self {
            state_dir,
            parent_session_id,
            ignored_children,
            unmatched_spawns: VecDeque::new(),
            children: HashMap::new(),
        }
    }

    pub(super) fn has_active_subagents(&self) -> bool {
        !self.unmatched_spawns.is_empty()
            || self
                .children
                .values()
                .any(|child| child.status == ItemStatus::InProgress)
    }

    pub(super) fn on_spawn(&mut self, spawn: SpawnedSubagent) {
        self.unmatched_spawns.push_back(spawn);
    }

    pub(super) fn on_spawn_failed(&mut self, tool_call_id: &str) {
        self.unmatched_spawns
            .retain(|spawn| spawn.tool_call_id != tool_call_id);
    }

    /// Close any still-running matched subagents and unmatched spawns as
    /// interrupted when the session closes so their mirrors do not stay open.
    pub(super) fn finish_all_interrupted(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        for child in self.children.values_mut() {
            if child.status == ItemStatus::InProgress {
                child.status = ItemStatus::Interrupted;
                let summary = child.tailer.summary();
                events.push(AgentEvent::ItemCompleted(
                    child.snapshot(ItemStatus::Interrupted, summary),
                ));
            }
        }
        for spawn in self.unmatched_spawns.drain(..) {
            events.push(AgentEvent::ItemCompleted(ThreadItem {
                id: spawn.tool_call_id,
                parent_item_id: None,
                content: ItemContent::Subagent {
                    agent_type: spawn.agent_type,
                    description: spawn.description,
                    status: ItemStatus::Interrupted,
                    summary: None,
                    model: spawn.model,
                    effort: None,
                },
            }));
        }
        events
    }

    pub(super) fn on_message_sent(
        &mut self,
        recipient: &str,
        message: Option<String>,
    ) -> Vec<AgentEvent> {
        let Some(child) = self.children.get_mut(recipient) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if child.status != ItemStatus::InProgress {
            child.status = ItemStatus::InProgress;
            child.tailer.turn_done = false;
            child.tailer.last_reported = None;
            events.push(AgentEvent::ItemUpdated(
                child.snapshot(ItemStatus::InProgress, None),
            ));
        }
        if let Some(text) = message {
            child.followup_seq += 1;
            events.push(AgentEvent::ItemCompleted(ThreadItem {
                id: format!("{}:followup-{}", child.tool_call_id, child.followup_seq),
                parent_item_id: Some(child.tool_call_id.clone()),
                content: ItemContent::UserMessage {
                    text,
                    attachments: Vec::new(),
                },
            }));
        }
        events
    }

    /// Discover newly spawned child files, tail transcripts, and emit child
    /// events followed by any parent `Subagent` status transitions.
    pub(super) fn poll(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let files = read_subagent_files(&self.state_dir, &self.parent_session_id);
        let mut states_by_id: HashMap<String, String> = HashMap::new();

        for file in files {
            states_by_id.insert(file.conversation_id.clone(), file.state.clone());
            if self.ignored_children.contains(&file.conversation_id)
                || self.children.contains_key(&file.conversation_id)
            {
                continue;
            }
            if let Some(spawn) = self.take_matching_spawn(&file) {
                let tailer = ChildTailer::new(
                    spawn.tool_call_id.clone(),
                    file.conversation_id.clone(),
                    &self.state_dir,
                    spawn.model.clone(),
                );
                self.children.insert(
                    file.conversation_id,
                    TrackedSubagent {
                        tool_call_id: spawn.tool_call_id,
                        agent_type: spawn.agent_type,
                        description: spawn.description,
                        status: ItemStatus::InProgress,
                        model: spawn.model,
                        tailer,
                        followup_seq: 0,
                    },
                );
            }
        }

        for (child_id, child) in &mut self.children {
            let model_before = child.tailer.model.clone();
            let child_events = child.tailer.poll();
            let model_changed = child.tailer.model != model_before;
            events.extend(child_events);

            if child.status != ItemStatus::InProgress {
                continue;
            }

            let killed = states_by_id
                .get(child_id)
                .is_some_and(|state| state == "SUBAGENT_STATE_KILLED");
            if child.tailer.turn_done {
                child.status = ItemStatus::Completed;
                let summary = child.tailer.summary();
                events.push(AgentEvent::ItemCompleted(
                    child.snapshot(ItemStatus::Completed, summary),
                ));
            } else if killed {
                child.status = ItemStatus::Interrupted;
                let summary = child.tailer.summary();
                events.push(AgentEvent::ItemCompleted(
                    child.snapshot(ItemStatus::Interrupted, summary),
                ));
            } else if child.tailer.failed_in_db() {
                child.status = ItemStatus::Failed;
                let summary = child.tailer.summary();
                events.push(AgentEvent::ItemCompleted(
                    child.snapshot(ItemStatus::Failed, summary),
                ));
            } else if model_changed {
                events.push(AgentEvent::ItemUpdated(
                    child.snapshot(ItemStatus::InProgress, None),
                ));
            }
        }

        events
    }

    fn take_matching_spawn(&mut self, file: &SubagentFile) -> Option<SpawnedSubagent> {
        let exact = self.unmatched_spawns.iter().position(|spawn| {
            spawn.agent_type == file.subagent_descriptor.type_name
                && spawn.description == file.subagent_descriptor.role
        });
        let index = exact.or_else(|| {
            file.subagent_descriptor.role.is_empty().then(|| {
                self.unmatched_spawns
                    .iter()
                    .position(|spawn| spawn.agent_type == file.subagent_descriptor.type_name)
            })?
        })?;
        self.unmatched_spawns.remove(index)
    }
}

fn read_subagent_files(state_dir: &Path, parent_session_id: &str) -> Vec<SubagentFile> {
    let dir = state_dir
        .join("brain")
        .join(parent_session_id)
        .join(".system_generated")
        .join("subagents");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<SubagentFile> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str::<SubagentFile>(&text).ok())
        .collect();
    files.sort_by(|a, b| {
        a.spawn_step_index
            .cmp(&b.spawn_step_index)
            .then_with(|| a.conversation_id.cmp(&b.conversation_id))
    });
    files
}

/// The task that follows one Antigravity session's subagents and the turns
/// the agent starts on its own: idle until told of one, then reading their
/// records every [`POLL_INTERVAL`] of the host's clock while one runs.
///
/// It runs on the session's own connection, the thread its notifications are
/// handled on, so what it reports never overtakes what the agent sent before
/// it: the files are read off that thread, and the session's state is changed
/// and the events sent back on it with nothing awaited in between.
pub(super) async fn run_watcher(
    state_dir: PathBuf,
    session_id: String,
    acp_state: Arc<Mutex<State>>,
    proactive: Arc<Mutex<ProactiveTracker>>,
    events: Sender<AgentEvent>,
    controls: Receiver<WatcherCommand>,
    clock: crate::Clock,
) {
    let mut watcher = {
        let (state_dir, session_id) = (state_dir.clone(), session_id.clone());
        crate::process::unblock(move || WatcherState::new(state_dir, session_id)).await
    };
    let db_path = record::path(&state_dir, &session_id);
    let transcript_path = record::transcript_path(&state_dir, &session_id);

    let mut closing = false;
    loop {
        let need_poll = watcher.has_active_subagents()
            || proactive
                .lock()
                .unwrap_or_else(|held| held.into_inner())
                .active
                .is_some();

        let next_command = if closing {
            None
        } else if need_poll {
            let due = clock.timer_at(clock.now() + POLL_INTERVAL);
            future::or(async { Some(controls.recv().await) }, async {
                due.await;
                None
            })
            .await
        } else {
            Some(controls.recv().await)
        };

        let mut immediate_events = Vec::new();
        let mut sync_seen = false;
        for cmd in next_command
            .into_iter()
            .chain(std::iter::from_fn(|| controls.try_recv().ok().map(Ok)))
        {
            match cmd {
                Ok(WatcherCommand::Spawned(spawn)) => watcher.on_spawn(spawn),
                Ok(WatcherCommand::SpawnFailed(id)) => watcher.on_spawn_failed(&id),
                Ok(WatcherCommand::MessageSent { recipient, message }) => {
                    immediate_events.extend(watcher.on_message_sent(&recipient, message));
                }
                Ok(WatcherCommand::SyncSeen) => sync_seen = true,
                Ok(WatcherCommand::Wake) => {}
                Ok(WatcherCommand::Close) | Err(_) => {
                    closing = true;
                }
            }
        }

        // Release `proactive` before locking `acp_state` so the watcher and
        // notification callbacks never invert lock order (`acp_state` -> `proactive`).
        let proactive_check = proactive
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .active
            .as_ref()
            .map(|active| (active.turn_id.clone(), active.since, active.usage_seen));
        let context = acp_state
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .usage();
        let (db, transcript) = (db_path.clone(), transcript_path.clone());
        let (polled_watcher, polled, ended, synced_step) = crate::process::unblock(move || {
            let polled = watcher.poll();
            let ended = proactive_check.and_then(|(turn_id, since, usage_seen)| {
                let end = if usage_seen {
                    record::proactive_turn_end(&db, &transcript, since, context)
                } else {
                    record::last_step(&db)
                        .filter(|last| last.error && since.is_none_or(|seen| last.index > seen))
                        .map(|last| record::ProactiveEnd {
                            step_index: last.index,
                            status: TurnStatus::Failed,
                            usage: context,
                        })
                };
                end.map(|end| (turn_id, end))
            });
            let synced_step = sync_seen
                .then(|| record::last_step(&db).map(|step| step.index))
                .flatten();
            (watcher, polled, ended, synced_step)
        })
        .await;
        watcher = polled_watcher;

        // Back on the session's thread: nothing is awaited from here to the
        // last event sent. Lock `acp_state` before `proactive` consistently.
        let mut out = immediate_events;
        out.extend(polled);
        {
            let mut state = acp_state.lock().unwrap_or_else(|held| held.into_inner());
            let mut tracker = proactive.lock().unwrap_or_else(|held| held.into_inner());
            if let Some(step) = synced_step
                && tracker.seen.is_none_or(|seen| step > seen)
            {
                tracker.seen = Some(step);
            }
            if let Some((turn_id, end)) = ended
                && tracker
                    .active
                    .as_ref()
                    .is_some_and(|active| active.turn_id == turn_id)
            {
                tracker.active = None;
                if state.turn() == Some(turn_id.as_str()) {
                    tracker.seen = Some(end.step_index);
                    drop(tracker);
                    out.extend(state.end_turn(
                        &turn_id,
                        TurnEnd {
                            status: end.status,
                            message: None,
                            usage: end.usage,
                        },
                    ));
                }
            }
        }
        if closing {
            out.extend(watcher.finish_all_interrupted());
        }
        for event in out {
            // The session's event channel is unbounded: a send fails only
            // once its reader is gone.
            if events.try_send(event).is_err() {
                return;
            }
        }

        if closing {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("tcode-agy-subagent-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write_subagent(
            &self,
            parent: &str,
            child: &str,
            type_name: &str,
            role: &str,
            state: &str,
            step: u64,
        ) {
            let dir = self
                .0
                .join("brain")
                .join(parent)
                .join(".system_generated")
                .join("subagents");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{child}.json")),
                json!({
                    "conversationId": child,
                    "subagentDescriptor": { "typeName": type_name, "role": role },
                    "state": state,
                    "spawnStepIndex": step,
                })
                .to_string(),
            )
            .unwrap();
        }

        fn append_transcript(&self, child: &str, line: &Value) {
            let path = record::transcript_path(&self.0, child);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(file, "{line}").unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_invoke_subagent_and_streams_child_transcript_to_completion() {
        let scratch = Scratch::new();
        let call: acp::ToolCall = serde_json::from_value(json!({
            "toolCallId": "call_sub_1",
            "title": "Run invoke_subagent",
            "kind": "other",
            "status": "in_progress",
            "rawInput": {
                "TypeName": "research",
                "Role": "Code Reader",
                "Prompt": "Read hello.txt and report back.",
                "Model": "inherit"
            }
        }))
        .unwrap();

        let spawn = parse_spawn(&call).expect("invoke_subagent should parse");
        assert_eq!(spawn.tool_call_id, "call_sub_1");
        assert_eq!(spawn.agent_type, "research");
        assert_eq!(spawn.description, "Code Reader");
        assert_eq!(spawn.model, None);

        let initial = spawn_events(&spawn);
        assert!(matches!(
            &initial[0],
            AgentEvent::ItemStarted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::Subagent {
                    agent_type,
                    description,
                    status: ItemStatus::InProgress,
                    ..
                }
            }) if id == "call_sub_1" && agent_type == "research" && description == "Code Reader"
        ));
        assert!(matches!(
            &initial[1],
            AgentEvent::ItemCompleted(ThreadItem {
                parent_item_id: Some(parent),
                content: ItemContent::UserMessage { text, .. },
                ..
            }) if parent == "call_sub_1" && text == "Read hello.txt and report back."
        ));

        let mut watcher = WatcherState::new(scratch.0.clone(), "parent-1".into());
        watcher.on_spawn(spawn);
        scratch.write_subagent(
            "parent-1",
            "child-1",
            "research",
            "Code Reader",
            "SUBAGENT_STATE_ALIVE",
            3,
        );
        scratch.append_transcript(
            "child-1",
            &json!({
                "step_index": 0,
                "type": "SYSTEM_MESSAGE",
                "status": "DONE",
                "content": "internal preamble"
            }),
        );
        scratch.append_transcript(
            "child-1",
            &json!({
                "step_index": 1,
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "model": "gemini-3.5-flash-lite",
                "thinking": "Inspecting the file",
                "tool_calls": [{
                    "name": "read_file",
                    "args": { "Path": "\"/repo/hello.txt\"" }
                }]
            }),
        );

        let first = watcher.poll();
        assert!(first.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem {
                parent_item_id: Some(parent),
                content: ItemContent::Reasoning { text },
                ..
            }) if parent == "call_sub_1" && text == "Inspecting the file"
        )));
        assert!(first.iter().any(|event| matches!(
            event,
            AgentEvent::ItemStarted(ThreadItem {
                parent_item_id: Some(parent),
                content: ItemContent::ToolCall {
                    name,
                    input,
                    family: Some(family),
                    ..
                },
                ..
            }) if parent == "call_sub_1"
                && name == "read_file"
                && input == &json!({ "Path": "/repo/hello.txt" })
                && matches!(family.as_ref(), ToolFamily::Read { path, .. } if path == "/repo/hello.txt")
        )));
        assert!(first.iter().any(|event| matches!(
            event,
            AgentEvent::ItemUpdated(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::Subagent {
                    status: ItemStatus::InProgress,
                    model: Some(model),
                    ..
                },
            }) if id == "call_sub_1" && model == "gemini-3.5-flash-lite"
        )));

        scratch.append_transcript(
            "child-1",
            &json!({
                "step_index": 2,
                "type": "GENERIC",
                "status": "DONE",
                "content": "hello world"
            }),
        );
        scratch.append_transcript(
            "child-1",
            &json!({
                "step_index": 3,
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "model": "gemini-3.5-flash-lite",
                "tool_calls": [{
                    "name": "send_message",
                    "args": {
                        "Recipient": "\"parent-1\"",
                        "Message": "\"The file says hello world\""
                    }
                }]
            }),
        );
        scratch.append_transcript(
            "child-1",
            &json!({
                "step_index": 4,
                "type": "GENERIC",
                "status": "DONE",
                "content": "Message sent"
            }),
        );
        scratch.append_transcript(
            "child-1",
            &json!({
                "step_index": 5,
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "model": "gemini-3.5-flash-lite",
                "content": "Reported to parent."
            }),
        );

        let second = watcher.poll();
        assert!(matches!(
            second.last(),
            Some(AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::Subagent {
                    status: ItemStatus::Completed,
                    summary: Some(summary),
                    model: Some(model),
                    ..
                },
            })) if id == "call_sub_1"
                && summary == "The file says hello world"
                && model == "gemini-3.5-flash-lite"
        ));
        assert!(!watcher.has_active_subagents());
    }

    #[test]
    fn killed_subagent_reports_interrupted_and_preserves_parallel_correlation() {
        let scratch = Scratch::new();
        let mut watcher = WatcherState::new(scratch.0.clone(), "parent-2".into());
        watcher.on_spawn(SpawnedSubagent {
            tool_call_id: "call_a".into(),
            agent_type: "research".into(),
            description: "Reader A".into(),
            prompt: None,
            model: None,
        });
        watcher.on_spawn(SpawnedSubagent {
            tool_call_id: "call_b".into(),
            agent_type: "research".into(),
            description: "Reader B".into(),
            prompt: None,
            model: None,
        });

        scratch.write_subagent(
            "parent-2",
            "child-b",
            "research",
            "Reader B",
            "SUBAGENT_STATE_KILLED",
            4,
        );
        scratch.write_subagent(
            "parent-2",
            "child-a",
            "research",
            "Reader A",
            "SUBAGENT_STATE_ALIVE",
            4,
        );

        let events = watcher.poll();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::Subagent {
                    status: ItemStatus::Interrupted,
                    description,
                    ..
                },
            }) if id == "call_b" && description == "Reader B"
        )));
        assert!(watcher.has_active_subagents());

        // Closing interrupts still-running subagents and clears pending work.
        let closing = watcher.finish_all_interrupted();
        assert!(matches!(
            closing.as_slice(),
            [AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::Subagent {
                    status: ItemStatus::Interrupted,
                    description,
                    ..
                },
            })] if id == "call_a" && description == "Reader A"
        ));
        assert!(!watcher.has_active_subagents());
    }

    #[test]
    fn failed_spawn_and_mismatched_type_do_not_cross_wire_or_wedge_watcher() {
        let scratch = Scratch::new();
        let mut watcher = WatcherState::new(scratch.0.clone(), "parent-3".into());
        watcher.on_spawn(SpawnedSubagent {
            tool_call_id: "call_explore".into(),
            agent_type: "explore".into(),
            description: "Task A".into(),
            prompt: None,
            model: None,
        });
        scratch.write_subagent(
            "parent-3",
            "child-other",
            "research",
            "Task A",
            "SUBAGENT_STATE_ALIVE",
            2,
        );
        // A different typeName must not grab `call_explore`.
        assert!(watcher.poll().is_empty());

        // A failed spawn is removed cleanly from `unmatched_spawns`.
        watcher.on_spawn_failed("call_explore");
        assert!(!watcher.has_active_subagents());

        // Even if an earlier child tool call had no `GENERIC` step, the final
        // `PLANNER_RESPONSE` drains it and settles the child.
        watcher.on_spawn(SpawnedSubagent {
            tool_call_id: "call_res".into(),
            agent_type: "research".into(),
            description: "Task A".into(),
            prompt: None,
            model: None,
        });
        scratch.append_transcript(
            "child-other",
            &json!({
                "step_index": 1,
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "tool_calls": [{ "name": "read_file", "args": {} }]
            }),
        );
        scratch.append_transcript(
            "child-other",
            &json!({
                "step_index": 2,
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "content": "Finished"
            }),
        );
        let events = watcher.poll();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::Subagent {
                    status: ItemStatus::Completed,
                    summary: Some(summary),
                    ..
                },
            }) if id == "call_res" && summary == "Finished"
        )));
    }
}
