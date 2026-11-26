//! What the machines of a group ask each other over `tcode/1`, and what a
//! client sees of a project moving to another machine.
use serde::{Deserialize, Serialize};

/// One request from another machine of the group, answered by one
/// [`MachineResponse`]. Only a [`crate::Principal::Machine`] may send it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum MachineRequest {
    Ping,
    /// Run one `tcode_control` tool here for a thread on the calling machine.
    Control {
        caller: MachineCaller,
        tool: String,
        args: serde_json::Value,
    },
    /// Text for a thread here from a thread this machine started on the
    /// calling machine.
    Callback {
        thread_id: String,
        from_thread_id: String,
        from_title: String,
        text: String,
        report: bool,
    },
    /// The calling machine's lead took the report of this machine's child.
    ReportTaken {
        child_thread_id: String,
    },
    /// Whether a project called `project_name` with these threads could
    /// move here, to `root` when one is given; answered with
    /// [`MachineResponse::MovePreflight`], or refused as `MoveBegin` would
    /// refuse it.
    MovePreflight {
        project_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
        thread_ids: Vec<String>,
    },
    /// Stage a move here: the threads it brings, by id, so one this machine
    /// already has is refused before any bytes, and the parts it sends.
    MoveBegin {
        move_id: String,
        project_name: String,
        root: String,
        git: bool,
        thread_ids: Vec<String>,
        parts: Vec<MovePart>,
    },
    MoveChunk {
        move_id: String,
        part: String,
        offset: u64,
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
    },
    MoveCommit {
        move_id: String,
    },
    MoveAbort {
        move_id: String,
    },
    MoveStatus {
        move_id: String,
    },
}

/// Which of a machine's switches a [`MachineRequest`] falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineRequestKind {
    Ping,
    /// A thread of the calling machine works here: **accept threads**.
    Thread,
    /// The answer to something this machine sent out, which comes back
    /// whatever the switches say.
    Reply,
    /// A project moves here: **accept projects**.
    Project,
}

impl MachineRequest {
    pub fn kind(&self) -> MachineRequestKind {
        match self {
            Self::Ping => MachineRequestKind::Ping,
            Self::Control { .. } => MachineRequestKind::Thread,
            Self::Callback { .. } | Self::ReportTaken { .. } => MachineRequestKind::Reply,
            Self::MovePreflight { .. }
            | Self::MoveBegin { .. }
            | Self::MoveChunk { .. }
            | Self::MoveCommit { .. }
            | Self::MoveAbort { .. }
            | Self::MoveStatus { .. } => MachineRequestKind::Project,
        }
    }
}

/// The thread on the calling machine a [`MachineRequest::Control`] acts for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineCaller {
    pub thread_id: String,
    pub title: String,
    /// The caller thread's permission mode as its harness reports it; opaque here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// One file a move sends, staged under its name until the commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovePart {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum MachineResponse {
    Pong {
        protocol_version: u32,
    },
    Control(serde_json::Value),
    Done,
    MovePreflight {
        /// Where the project would go here: this machine's folder for new
        /// projects joined with the project's name, made unique.
        default_root: String,
    },
    /// The bytes already staged of each part, for a move that resumes.
    MoveBegun {
        received: Vec<(String, u64)>,
    },
    MoveProgress {
        received: u64,
    },
    MoveCommitted {
        project_id: String,
    },
    MoveState(MoveState),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum MoveState {
    Unknown,
    Staging,
    Committed { project_id: String },
}

/// What moving a project to another machine would take, and what stands in
/// its way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMovePreview {
    /// Where the project would go on the other machine unless another root
    /// is chosen.
    pub default_root: String,
    pub git: bool,
    pub branches: u32,
    pub bytes: u64,
    pub dirty_checkouts: u32,
    pub threads: u32,
    pub archived_threads: u32,
    /// Files git ignores that a move leaves behind unless they are included.
    pub ignored_files: Vec<String>,
    pub blockers: Vec<MoveBlocker>,
    /// The other machine is reached through a relay.
    pub relayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum MoveBlocker {
    /// The project holds the quick chats, which stay with their machine.
    QuickChat,
    ThreadsRunning(u32),
    /// Approvals and questions waiting for an answer.
    PendingApprovals(u32),
    /// Threads with messages queued.
    QueuedMessages(u32),
    TerminalsOpen(u32),
    MergeInProgress {
        path: String,
    },
    TargetRefuses,
    TargetUnreachable,
    TargetVersion,
}

/// One project moving away from this machine, as
/// [`crate::Topic::ProjectMoves`] lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMoveStatus {
    pub move_id: String,
    pub project_id: String,
    pub machine: String,
    pub phase: MovePhase,
    /// Bytes the other machine holds of all the parts, and their sum; both 0
    /// while the parts are being built.
    pub done: u64,
    pub total: u64,
    /// The commit was sent and its outcome is not known yet: the move can
    /// only be cancelled with `force`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub commit_unknown: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovePhase {
    Preparing,
    SendingCode,
    SendingThreads,
    Committing,
    Paused,
    Cancelling,
}
