//! Records of the `tcode_control` surface: what a thread's model did in Tcode
//! through it, and what its calls cover.

use std::collections::BTreeSet;

pub use agent::ControlAction;
use serde::{Deserialize, Serialize};

/// Which projects a listing or a search covers. `Project` is the calling thread's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ControlScope {
    Project,
    Projects(Vec<String>),
    Host,
}

impl ControlScope {
    /// Whether it covers `target_project` for a caller in `caller_project`. A thread without
    /// a project is covered only by the whole host, or by `Project` for a caller that also
    /// has none.
    pub fn reaches(&self, caller_project: Option<&str>, target_project: Option<&str>) -> bool {
        match self {
            Self::Host => true,
            Self::Project => caller_project == target_project,
            Self::Projects(ids) => target_project.is_some_and(|id| ids.iter().any(|p| p == id)),
        }
    }
}

/// The account profiles and models something may run: a space's members, or the threads
/// `tcode_control` starts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum AgentScope {
    /// Every enabled account profile and model, including ones added later.
    #[default]
    Every,
    /// Only these profiles; one added later is not among them.
    Only(Vec<AgentGrant>),
}

impl AgentScope {
    pub fn is_every(&self) -> bool {
        *self == Self::Every
    }

    /// The grant for `profile_id`, when the scope holds that profile at all.
    pub fn agent(&self, profile_id: &str) -> Option<AgentAccess<'_>> {
        match self {
            Self::Every => Some(AgentAccess::Every),
            Self::Only(grants) => grants
                .iter()
                .find(|grant| grant.profile_id == profile_id)
                .map(AgentAccess::Granted),
        }
    }
}

/// One account profile an [`AgentScope`] holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGrant {
    pub profile_id: String,
    pub models: ModelGrant,
    /// For the ACP profile, the registry ids of the agents the scope holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acp_agents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ModelGrant {
    /// Every model the profile offers, including ones it offers later.
    Every,
    Only(Vec<String>),
}

/// What [`AgentScope::agent`] found for one profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAccess<'a> {
    Every,
    Granted(&'a AgentGrant),
}

impl AgentAccess<'_> {
    pub fn allows_model(&self, model: Option<&str>) -> bool {
        match self {
            Self::Every => true,
            Self::Granted(grant) => match &grant.models {
                ModelGrant::Every => true,
                ModelGrant::Only(models) => {
                    model.is_some_and(|model| models.iter().any(|allowed| allowed == model))
                }
            },
        }
    }

    pub fn allows_acp_agent(&self, agent: Option<&str>) -> bool {
        match self {
            Self::Every => true,
            Self::Granted(grant) => {
                agent.is_some_and(|agent| grant.acp_agents.iter().any(|allowed| allowed == agent))
            }
        }
    }
}

/// Whether a thread's model is given `tcode_control`, and which accounts and models the
/// threads it starts may run. What it then does with it is the harness's to allow or ask
/// about, as with any other tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "AgentScope::is_every")]
    pub agents: AgentScope,
    /// Whether this machine's threads may start threads on the other machines of its group.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub machines: bool,
}

fn is_true(value: &bool) -> bool {
    *value
}

const fn default_true() -> bool {
    true
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            agents: AgentScope::Every,
            machines: true,
        }
    }
}

impl ControlSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// How far Tcode goes on its own with settlement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementAutomation {
    Off,
    Suggest,
    #[default]
    ExplicitSignals,
}

impl SettlementAutomation {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowUp {
    /// Unix seconds.
    pub since: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SelfSettleWhen {
    EndOfTurn,
    PrMerged { url: String },
    PrClosed { url: String },
}

/// The user's consent, quoted, for the thread to settle itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfSettleGrant {
    pub when: SelfSettleWhen,
    pub consent_quote: String,
    /// Unix seconds.
    pub granted_at: u64,
}

/// The thread and idempotency key that started this thread through `start_thread`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartedBy {
    pub thread_id: String,
    pub request_id: String,
    /// The machine of the group `thread_id` is on, when it is not this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// The thread on another machine of the group that started this one as its subagent. This
/// thread is an ordinary root thread here; only its report goes back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteParent {
    pub machine: String,
    pub thread_id: String,
    pub title: String,
}

/// A thread this thread started on another machine of the group: a reference and its last
/// known state, never its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteChild {
    pub machine: String,
    pub thread_id: String,
    pub title: String,
    pub request_id: String,
    /// Started as a subagent, which owes a report.
    pub subagent: bool,
    pub state: RemoteChildState,
    /// Unix seconds of the last change to `state`.
    pub updated_at: u64,
    /// What this machine still has to tell the other one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<RemoteChildOp>,
    /// The id of the child's report the lead's harness took last, so the same report sent
    /// again is known as taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_taken: Option<String>,
    /// The other machine has not yet heard that `report_taken` was taken, and keeps the
    /// report until it does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub report_taken_unsent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteChildState {
    Running,
    Reported,
    Failed,
    Cancelled,
    /// Its lead settled it.
    Settled,
    /// The machine left the group.
    MachineLeft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteChildOp {
    Interrupt,
    Cancel,
    Settle,
}

/// A wake a thread on another machine of the group asked for on this thread's events, kept
/// on this thread. It fires once, and its firing is kept until that machine has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWatch {
    pub id: String,
    pub machine: String,
    /// The watching thread, on `machine`.
    pub thread_id: String,
    pub events: BTreeSet<WatchEvent>,
    /// The message its firing sends, until `machine` has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fired: Option<String>,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WatchEvent {
    Idle,
    TurnCompleted,
    NeedsInput,
    NeedsApproval,
    Settled,
    PrChanged,
}

/// A wake the caller asked for on other threads' events. Each thread in it wakes the caller
/// once and then leaves it, so a watch nobody cancels cannot keep waking a thread for good.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlWatch {
    pub id: String,
    pub thread_ids: Vec<String>,
    pub events: BTreeSet<WatchEvent>,
    /// The space whose member's turn asked for it: what it wakes runs under
    /// that space's policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// The machine of the group `thread_ids` are on, when it is not this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// The semantic keys a [`ControlAction`] records, shared by the host that writes them and the
/// clients that put them in words. A [`ControlAction::detail`] is data only (a title, an id,
/// a name, a count, the user's or the model's own words), never a sentence.
pub mod action {
    pub const SETTLE: &str = "settle";
    /// The thread settled itself under the user's consent; detail: the consent quote.
    pub const SETTLED_ITSELF: &str = "settled_itself";
    pub const UNSETTLE: &str = "unsettle";
    pub const ARCHIVE: &str = "archive";
    pub const UNARCHIVE: &str = "unarchive";
    pub const PIN: &str = "pin";
    pub const UNPIN: &str = "unpin";
    /// Detail: the new title.
    pub const RENAME: &str = "rename";
    pub const MARK_READ: &str = "mark_read";
    pub const MARK_UNREAD: &str = "mark_unread";
    /// Detail: the note, if any.
    pub const FOLLOW_UP: &str = "follow_up";
    pub const FOLLOW_UP_OFF: &str = "follow_up_off";
    pub const PARK: &str = "park";
    pub const PARK_OFF: &str = "park_off";
    pub const KEEP: &str = "keep";
    pub const KEEP_OFF: &str = "keep_off";
    /// Detail: the consent quote.
    pub const SETTLE_SELF: &str = "settle_self";
    /// Detail: the watch id.
    pub const NOTIFY_WHEN: &str = "notify_when";
    /// Detail: the removed watch ids.
    pub const CANCEL_NOTIFY: &str = "cancel_notify";
    /// Detail: the request id.
    pub const RESPOND_INPUT: &str = "respond_input";
    /// Detail: the approval id.
    pub const RESPOND_APPROVAL: &str = "respond_approval";
    pub const TERMINAL_OPEN: &str = "terminal_open";
    /// Detail: the terminal id.
    pub const TERMINAL_INPUT: &str = "terminal_input";
    /// Detail: the terminal id.
    pub const TERMINAL_CLOSE: &str = "terminal_close";
    /// Detail: the deleted thread's title.
    pub const DELETE_THREADS: &str = "delete_threads";
    /// Detail: the turn id.
    pub const REWIND: &str = "rewind";
    /// Detail: the setting's key.
    pub const PATCH_SETTINGS: &str = "patch_settings";
    /// Detail: the model's id.
    pub const SET_MODEL_GUIDE: &str = "set_model_guide";
    pub const START_THREAD: &str = "start_thread";
    /// Detail: the target project's name and root.
    pub const MOVE: &str = "move";
    pub const SEND_QUEUED: &str = "send_queued";
    pub const SEND_STEERED: &str = "send_steered";
    pub const SEND_SCHEDULED: &str = "send_scheduled";
    pub const INTERRUPT: &str = "interrupt";
    /// A subagent stopped by the thread that started it, its result undelivered.
    pub const CANCEL: &str = "cancel";
    /// Detail: the turn id.
    pub const RETRY: &str = "retry";
    pub const COMPACT: &str = "compact";
    /// Detail: the model id.
    pub const SET_MODEL: &str = "set_model";
    /// Detail: the option id.
    pub const SET_OPTION: &str = "set_option";
    pub const FORK: &str = "fork";
    /// Detail: the queued message id.
    pub const STEER_QUEUED: &str = "steer_queued";
    /// Detail: the queued message id.
    pub const DROP_QUEUED: &str = "drop_queued";
    /// The pull request operations; detail: the pull request.
    pub const PR_LINK: &str = "pr_link";
    pub const PR_UNLINK: &str = "pr_unlink";
    pub const PR_WATCH: &str = "pr_watch";
    pub const PR_UNWATCH: &str = "pr_unwatch";
    pub const PR_REFRESH: &str = "pr_refresh";
    pub const PR_COMMENT: &str = "pr_comment";
    pub const PR_REPLY: &str = "pr_reply";
    pub const PR_RESOLVE: &str = "pr_resolve";
    /// Detail: the branch.
    pub const GIT_CHECKOUT: &str = "git_checkout";
    pub const GIT_MERGE_WORKTREE: &str = "git_merge_worktree";
    pub const GIT_COMMIT: &str = "git_commit";
    pub const GIT_COMMIT_PUSH: &str = "git_commit_push";
    pub const GIT_PUSH: &str = "git_push";
    pub const GIT_PULL: &str = "git_pull";
    pub const GIT_PUBLISH_BRANCH: &str = "git_publish_branch";
    pub const GIT_INITIALIZE: &str = "git_initialize";
    /// Detail: the project root.
    pub const CREATE_PROJECT: &str = "create_project";
}

/// One `tcode_control` write, as Settings → Control lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlActivityEntry {
    pub target_thread_id: String,
    pub target_title: String,
    /// Unix milliseconds.
    pub at: u64,
    pub action: ControlAction,
}
