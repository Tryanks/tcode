//! The read side of a linked pull request, as the host's GitHub reads answer it.

use agent::FileChangeKind;
use serde::{Deserialize, Serialize};
use tcode_core::session::ReviewSide;

/// The largest image a media read returns; past it the host stops reading.
pub const MAX_PULL_REQUEST_MEDIA_BYTES: usize = 8 * 1024 * 1024;

/// One read of a pull request linked to the named thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestRead {
    /// `None` asks for the whole diff; a page is asked for only after a reply named it.
    Files {
        page: Option<u32>,
    },
    /// A file's text at an immutable revision, read when its diff is expanded.
    FileText {
        revision: String,
        path: String,
    },
    Conversation,
    /// A review thread's replies after the page the conversation carried.
    ThreadReplies {
        thread_id: String,
        after: String,
    },
    ViewedFiles,
    /// Media the pull request's conversation points at on GitHub, or an author's avatar.
    /// `validator` is the one a previous answer carried, to revalidate a cached copy.
    Media {
        url: String,
        validator: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestFiles {
    /// The commits the changed files are read between.
    pub base: String,
    pub head: String,
    pub files: Vec<PullRequestFile>,
    /// GitHub refused or cut the whole diff and pages its changed files instead.
    pub next_page: Option<u32>,
    /// Every changed file has been listed once the pages before this one were read too.
    pub complete: bool,
    /// GitHub's count of changed files, which a listing may fall short of.
    pub changed_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestFile {
    /// Repository-relative.
    pub path: String,
    pub previous_path: Option<String>,
    pub kind: FileChangeKind,
    pub additions: u64,
    pub deletions: u64,
    pub patch: PullRequestPatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestPatch {
    /// Unified hunks from the first `@@`; empty for a rename or mode change alone.
    Hunks(String),
    Binary,
    /// GitHub withheld the hunks of a file too large to show.
    Oversized,
    /// GitHub listed the file without hunks or counts: binary, or past its own diff limits,
    /// which the files listing does not tell apart.
    Withheld,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestFileText {
    Text(String),
    Binary,
    Oversized,
    /// The file does not exist at that revision.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestActor {
    pub login: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReaction {
    /// GitHub's reaction name, such as `THUMBS_UP`.
    pub content: String,
    pub count: u64,
    pub viewer_reacted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestComment {
    pub id: String,
    /// Absent for a deleted account.
    pub author: Option<PullRequestActor>,
    pub body: String,
    pub created_at: String,
    pub edited_at: Option<String>,
    pub url: Option<String>,
    /// Set for a review's own body.
    pub review_state: Option<PullRequestReviewState>,
    pub reactions: Vec<PullRequestReaction>,
}

/// Where a review thread was left: lines of one side of a file at one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewAnchor {
    pub revision: String,
    pub path: String,
    pub side: ReviewSide,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewThread {
    pub id: String,
    pub path: String,
    pub resolved: bool,
    /// The head moved past the lines; `anchor` then names the lines as first commented on.
    pub outdated: bool,
    /// Absent for a comment on a whole file.
    pub anchor: Option<PullRequestReviewAnchor>,
    /// The last lines of the hunk the thread was left on, without its `@@` header.
    pub diff_hunk: Option<String>,
    pub comments: Vec<PullRequestComment>,
    pub total_comments: u64,
    /// Where [`PullRequestRead::ThreadReplies`] carries on, while replies remain unread.
    pub replies_after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestConversation {
    pub description: PullRequestComment,
    /// Issue comments and review bodies, oldest first.
    pub comments: Vec<PullRequestComment>,
    pub threads: Vec<PullRequestReviewThread>,
    /// False when a list ran past the pages the host reads.
    pub complete: bool,
    /// Opaque, and different for each GitHub account the host reads as: a client keys the
    /// media this conversation shows by it, so no copy outlives an account change.
    pub account: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestThreadReplies {
    pub comments: Vec<PullRequestComment>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestViewedState {
    Viewed,
    Unviewed,
    /// Viewed before a later push changed the file.
    Dismissed,
}

/// The signed-in account's viewed marks. A path missing from an incomplete list is unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestViewedFiles {
    pub files: Vec<(String, PullRequestViewedState)>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestMedia {
    Image {
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
        mime: String,
        validator: Option<String>,
        /// Unix seconds after which a cached copy is read again.
        expires_at: u64,
    },
    /// The copy behind the request's validator is still current.
    NotModified { expires_at: u64 },
    /// Video and audio open in the browser until a range-capable read exists.
    External { mime: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestReadResponse {
    Files(PullRequestFiles),
    FileText(PullRequestFileText),
    Conversation(PullRequestConversation),
    ThreadReplies(PullRequestThreadReplies),
    ViewedFiles(PullRequestViewedFiles),
    Media(PullRequestMedia),
}
