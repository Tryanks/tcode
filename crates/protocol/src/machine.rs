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
        /// The id of the report the text carries, by which a report sent again is known;
        /// `None` for a question, an approval or a watch firing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report: Option<String>,
        /// What became of the subagent, when the text says it finished.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<tcode_core::control::RemoteChildState>,
        /// The id of the watch the text is the firing of.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        watch: Option<String>,
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
    Pong,
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
    /// The project's folder lies inside a larger repository: its history and its threads'
    /// worktrees belong to that repository, which is not this project's to move.
    InsideRepository,
    ThreadsRunning(u32),
    /// Approvals and questions waiting for an answer.
    PendingApprovals(u32),
    /// Threads with messages queued.
    QueuedMessages(u32),
    TerminalsOpen(u32),
    MergeInProgress {
        path: String,
    },
    /// A thread works in `path`, which the move would not carry: a worktree
    /// that is gone or no longer a checkout of this project's repository, or
    /// a place outside the project. `thread` is its title.
    MissingCheckout {
        thread: String,
        path: String,
    },
    TargetRefuses,
    TargetUnreachable,
    TargetVersion,
}

/// What [`crate::Topic::ProjectMoves`] holds: the moves under way, and how each
/// move that ended in this host's run ended, so a client that attaches or
/// reconnects late still learns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMovesState {
    pub moving: Vec<ProjectMoveStatus>,
    pub ended: Vec<EndedProjectMove>,
}

/// A move that ended on this machine. The host keeps it until it stops: a move
/// is rare and the record is a few strings, so nothing counts or ages it out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndedProjectMove {
    pub move_id: String,
    pub project_id: String,
    pub project_name: String,
    /// The machine the project was moving to.
    pub machine: String,
    /// The threads that went with it, or would have gone.
    pub thread_ids: Vec<String>,
    pub outcome: MoveOutcome,
}

/// How a move ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum MoveOutcome {
    /// The other machine has the project and its threads; this machine let go
    /// of them, and is letting go of the rest of what the move left.
    Moved,
    Cancelled {
        /// Cancelled once the commit had gone out with no answer, so the
        /// project may be on both machines.
        forced: bool,
    },
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
    /// Why the move waits, while it is paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<MovePause>,
}

/// Why a move is paused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum MovePause {
    /// The other machine cannot be reached; the move tries again by itself.
    Unreachable,
    /// The other machine no longer takes projects; the move tries again by
    /// itself.
    Refused,
    /// This machine reaches no group now; the move tries again by itself.
    NoGroup,
    /// The other machine runs another protocol version.
    Version,
    /// The other machine is no longer in this machine's group.
    NotMember,
    /// Something else failed, here or on the other machine, as it said.
    Failed { message: String },
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
