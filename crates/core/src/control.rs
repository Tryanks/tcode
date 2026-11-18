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

/// Whether a thread's model is given `tcode_control`. What it then does with it is the
/// harness's to allow or ask about, as with any other tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

const fn default_true() -> bool {
    true
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self { enabled: true }
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
    /// Detail: the number of items.
    pub const RECORD_REVIEW: &str = "record_review";
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewItem {
    pub thread_id: String,
    pub group: String,
    /// `clear_evidence`, `inference` or `unsure`.
    pub confidence: String,
    /// Anchors and quoted facts the group rests on.
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub id: String,
    /// The thread whose model recorded it.
    pub thread_id: String,
    /// Unix seconds.
    pub recorded_at: u64,
    /// What the review looked at, in its own words.
    pub coverage: String,
    pub items: Vec<ReviewItem>,
}
