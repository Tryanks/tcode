//! Localization-free runtime events presented by the UI.

use agent::ProviderKind;
use tcode_core::git::GitAction;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeEvent {
    Error(RuntimeError),
    Notice(RuntimeNotice),
    Toast(RuntimeToast),
    Effect(RuntimeEffect),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeEffect {
    /// Apply the persisted language override at the localization-aware UI boundary.
    ApplyLocale { language: Option<String> },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RuntimeOperationId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitActionRequest {
    pub action: GitAction,
    pub message: Option<String>,
    pub included: Option<Vec<String>>,
    pub feature_branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
    CheckpointRevertBlocked,
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
    CheckpointReverted,
    EditWithoutCheckpoint,
    PlanSaved {
        file: String,
    },
    SwitchedBranch {
        branch: String,
    },
}
