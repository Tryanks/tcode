use agent::{AgentEvent, ProviderKind, RewindMode};
use serde::{Deserialize, Serialize};
use tcode_core::{
    git::GitAction,
    project::{Project, SessionMeta},
    settings::Settings,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventRecord {
    pub ts: Option<u64>,
    pub event: AgentEvent,
}

impl PartialEq for SessionEventRecord {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Topic {
    SessionEvents { session_id: String },
    Index,
    Settings,
    RuntimeEvents,
    Terminal { terminal_id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub topic: Topic,
    pub seq: u64,
    pub event: ServerEvent,
}

impl PartialEq for EventEnvelope {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ServerEvent {
    SessionEvent(SessionEventRecord),
    IndexUpsertSession(SessionMeta),
    IndexUpsertProject(Project),
    IndexRemoveSession {
        session_id: String,
    },
    IndexRemoveProject {
        project_id: String,
    },
    SettingsReplaced(Settings),
    Runtime(RuntimeNotification),
    TerminalOutput {
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
    },
    TerminalExit {
        exit_code: Option<i32>,
    },
    SessionSnapshot(Vec<SessionEventRecord>),
    IndexSnapshot(IndexSnapshot),
    SettingsSnapshot(Settings),
    RuntimeSnapshot(RuntimeSnapshot),
    TerminalSnapshot(TerminalSnapshot),
}

impl PartialEq for ServerEvent {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexSnapshot {
    pub sessions: Vec<SessionMeta>,
    pub projects: Vec<Project>,
}

impl PartialEq for IndexSnapshot {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub notifications: Vec<RuntimeNotification>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    #[serde(with = "crate::wire::base64_bytes")]
    pub bytes: Vec<u8>,
    pub exit_code: Option<i32>,
}

/// Protocol-owned mirror of `tcode_runtime::event::RuntimeEvent`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeNotification {
    Error(RuntimeError),
    Notice(RuntimeNotice),
    Toast(RuntimeToast),
    Effect(RuntimeEffect),
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeEffect {
    ApplyLocale { language: Option<String> },
    CopyToClipboard { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeOperationId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitActionRequest {
    pub action: GitAction,
    pub message: Option<String>,
    pub included: Option<Vec<String>>,
    pub feature_branch: Option<String>,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeToast {
    GitBusy,
    GitStarted {
        operation: RuntimeOperationId,
        action: GitAction,
    },
    GitSucceeded {
        operation: RuntimeOperationId,
        action: GitAction,
    },
    GitFailed {
        operation: RuntimeOperationId,
        detail: String,
        retry: GitActionRequest,
    },
    CommitMessageGenerated {
        message: String,
    },
    CommitMessageFailed {
        detail: String,
    },
    AcpInstallStarted {
        operation: RuntimeOperationId,
        name: String,
    },
    AcpInstallSucceeded {
        operation: RuntimeOperationId,
        name: String,
    },
    AcpInstallFailed {
        operation: RuntimeOperationId,
        name: String,
        detail: String,
    },
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeError {
    External(String),
    PersistSettings { error: String },
    UpdateUnknown { provider: ProviderKind },
    UpdateFailed { provider: ProviderKind },
    TerminalStart { error: String },
    TerminalRestart { error: String },
    PersistProject { error: String },
    WorktreeRemove { error: String },
    DeleteSession { error: String },
    DeleteProject { error: String },
    NativeRewindBlocked,
    PersistEvent { error: String },
    WorktreeAdd { error: String },
    PersistSession { error: String },
    ProcessGone,
    SteerUnsupported { agent: String },
    DirtyTree,
    ProviderStart { error: String },
    ProviderClosed { reason: Option<String> },
    PersistSessionIndex { error: String },
    ProviderMessage(String),
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeNotice {
    ProviderMessage(String),
    UpdateAvailable {
        provider: ProviderKind,
        version: String,
    },
    UpdatingProvider {
        provider: ProviderKind,
    },
    UpdateDone {
        provider: ProviderKind,
    },
    NativeRewindCompleted {
        mode: RewindMode,
    },
    PlanSaved {
        file: String,
    },
    SwitchedBranch {
        branch: String,
    },
}
