//! The pull request host boundary: everything the runtime asks of a host that keeps pull
//! requests. A host's ids, cursors, query language, credentials and error shapes stay behind
//! it; what crosses is Tcode's own model. GitHub is the one host behind it. Call blocking
//! entries via HostCx::unblock.

use crate::settings::SettingsStore;
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tcode_core::{
    pull_request::{
        HostTerms, PullRequestKey, PullRequestMergeMethod, PullRequestReviewDraftComment,
        PullRequestSnapshot, PullRequestStackState, StackRebaseStep,
    },
    pull_request_watch::{PullRequestRemark, PullRequestWatchRead},
    session::ReviewSide,
    settings::{GitHubCredentialStatus, GitHubHostSettings},
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestRead,
    PullRequestReadResponse, PullRequestRejection as Rejection, PullRequestReviewVerdict,
    PullRequestStackHead,
};

/// The hosts Tcode reads pull requests from, with credentials from `store` and the launch
/// environment.
pub fn connect(
    store: SettingsStore,
    environment: impl IntoIterator<Item = (String, String)>,
) -> Arc<dyn Forge> {
    crate::github::GitHub::new(crate::github::GitHubApi::host(
        crate::github::Credentials::new(store, environment),
    ))
}

/// A pull request host. Each entry answers for the host the key names.
pub trait Forge: Send + Sync {
    /// What Tcode's text says about the host of the pull request.
    fn terms(&self, key: &PullRequestKey) -> &'static HostTerms;

    /// A host name as settings keep it, or `None` for one the host cannot be.
    fn normalize_host(&self, host: &str) -> Option<String>;
    fn configure(&self, hosts: BTreeMap<String, GitHubHostSettings>);
    /// Where each configured host's credential comes from, read now.
    fn credential_status(&self) -> BTreeMap<String, GitHubCredentialStatus>;
    /// Drops what is held of the host's credential, so the next request resolves it again.
    fn forget_credential(&self, host: &str);

    /// The key and canonical URL of a pull request's web URL, or `None` for any other URL.
    fn pull_request_url(&self, url: &str) -> Option<(PullRequestKey, String)>;
    /// The repository a checkout's remote names on the host.
    fn checkout_repository(&self, cwd: &Path) -> Option<Repository>;
    /// A repository named as the host writes it, on `host`.
    fn repository(&self, name: &str, host: &str) -> Option<Repository>;
    /// The web URL of a pull request.
    fn url(&self, key: &PullRequestKey) -> Option<String>;
    /// The pull request of the branch checked out at `cwd`, when `root`'s repository has one:
    /// read again with `refresh`, otherwise possibly from what was read recently.
    fn discover(&self, cwd: &Path, root: &Path, refresh: bool) -> Option<Discovered>;

    fn summary(&self, key: &PullRequestKey) -> Result<Summary, ForgeError>;
    fn stack(&self, key: &PullRequestKey) -> Result<PullRequestStackState, ForgeError>;

    /// One read of the pull request and when it should be read again.
    fn read(
        &self,
        key: &PullRequestKey,
        read: PullRequestRead,
    ) -> Result<(PullRequestReadResponse, SystemTime), ForgeError>;
    /// Drops every read held of the pull request.
    fn invalidate(&self, key: &PullRequestKey);
    /// Opaque, and different for each account the host reads as.
    fn account(&self, key: &PullRequestKey) -> Result<String, ForgeError>;
    fn set_viewed(
        &self,
        key: &PullRequestKey,
        paths: &[String],
        viewed: bool,
    ) -> Result<(), ForgeError>;

    fn act(&self, key: &PullRequestKey, action: &PullRequestAction) -> Outcome;
    /// Sends a review in one submission, refused when the head is no longer `head`.
    fn submit_review(
        &self,
        key: &PullRequestKey,
        verdict: PullRequestReviewVerdict,
        head: &str,
        body: &str,
        comments: &[PullRequestReviewDraftComment],
    ) -> Outcome;
    /// Whether the host would take a review comment on those lines at `head`.
    fn commentable(
        &self,
        key: &PullRequestKey,
        head: &str,
        path: &str,
        side: ReviewSide,
        lines: (u32, u32),
    ) -> Result<Anchoring, ForgeError>;
    /// The current head, and the revision each pending comment's lines read at there.
    fn reanchor(
        &self,
        key: &PullRequestKey,
        comments: &[PullRequestReviewDraftComment],
    ) -> Result<(String, Vec<Moved>), ForgeError>;

    /// Submits the host's asynchronous merge of native stack `stack` up to this layer. A host
    /// without native stacks has none to merge.
    fn merge_stack(
        &self,
        _key: &PullRequestKey,
        _stack: u64,
        _heads: &[PullRequestStackHead],
        _method: PullRequestMergeMethod,
    ) -> MergeSubmission {
        MergeSubmission::Done(Outcome::Rejected(Rejection::Invalid))
    }
    /// What became of stack merge `id`: `None` while it is still pending.
    fn merge_status(
        &self,
        _key: &PullRequestKey,
        _id: &str,
    ) -> Result<Option<Outcome>, ForgeError> {
        Err(ForgeError {
            kind: ForgeErrorKind::NotFound,
            description: "no native stacks on this host".into(),
        })
    }
    fn plan_stack_rebase(
        &self,
        _key: &PullRequestKey,
        _stack: u64,
        _heads: &[PullRequestStackHead],
    ) -> Result<StackRebase, Rejection> {
        Err(Rejection::Invalid)
    }

    /// One fingerprint per key, in order. `Ok(None)` is a pull request the host gave none for,
    /// as every one is on a host without fingerprints; it takes the reads gated by its sync
    /// snapshot.
    fn fingerprints(
        &self,
        keys: &[PullRequestKey],
    ) -> Vec<Result<Option<Fingerprint>, ForgeError>> {
        keys.iter().map(|_| Ok(None)).collect()
    }
    /// What a watch evaluates of the pull request now.
    fn watch_detail(&self, key: &PullRequestKey) -> Result<PullRequestWatchRead, ForgeError>;
    /// Every remark on the pull request, or `Ok(None)` when one could be missing. `tails` holds
    /// review-thread replies past a thread's first page between reads.
    fn activity(
        &self,
        key: &PullRequestKey,
        tails: &mut Tails,
    ) -> Result<Option<Vec<PullRequestRemark>>, ForgeError>;
}

/// Why a host could not answer, in Tcode's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeError {
    pub kind: ForgeErrorKind,
    /// The host's own description, which carries no request values.
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeErrorKind {
    /// Turned off for this host in settings.
    HostDisabled,
    /// No usable credential for the host: none saved or in the environment, and the host's CLI
    /// that would supply one is missing or signed out.
    NoCredential,
    /// The host refused the credential.
    Unauthorized,
    /// Refused before any request while an earlier rate limit lasts.
    Paused {
        retry_at: SystemTime,
    },
    RateLimited {
        retry_at: SystemTime,
    },
    NotFound,
    /// The host refused, in its own words.
    Refused {
        messages: Vec<String>,
    },
    /// No answer arrived, or none that could be read: a write may have been applied.
    Uncertain,
    Deadline,
    TooLarge,
    /// A read Tcode refuses before any request: an invalid revision, path or media URL.
    InvalidInput,
    /// Media whose type is not an image, video or audio, or an image that does not decode.
    UnsupportedMedia,
}

impl std::fmt::Display for ForgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.description)
    }
}
impl std::error::Error for ForgeError {}

impl ForgeError {
    /// When a rate limit or the pause after one ends.
    pub fn retry_at(&self) -> Option<SystemTime> {
        match self.kind {
            ForgeErrorKind::Paused { retry_at } | ForgeErrorKind::RateLimited { retry_at } => {
                Some(retry_at)
            }
            _ => None,
        }
    }

    /// Why a write was not sent, or why the host refused it.
    pub fn rejection(self) -> Rejection {
        match self.kind {
            ForgeErrorKind::HostDisabled => Rejection::HostDisabled,
            ForgeErrorKind::NoCredential | ForgeErrorKind::Unauthorized => Rejection::NoCredential,
            ForgeErrorKind::Paused { retry_at } | ForgeErrorKind::RateLimited { retry_at } => {
                Rejection::RateLimited {
                    retry_at: retry_at
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                }
            }
            ForgeErrorKind::NotFound => Rejection::NotFound,
            ForgeErrorKind::Refused { messages } => Rejection::Refused { messages },
            ForgeErrorKind::InvalidInput => Rejection::Invalid,
            _ => Rejection::Failed,
        }
    }
}

/// A repository on a host, as pull request keys locate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub host: String,
    pub locator: String,
}
impl Repository {
    pub fn key(&self, number: u64) -> PullRequestKey {
        PullRequestKey::new(&self.host, &self.locator, number)
    }
}

/// A pull request found for a checkout's branch.
#[derive(Debug, Clone)]
pub struct Discovered {
    /// The local branch it was found for.
    pub branch: String,
    pub key: PullRequestKey,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub snapshot: PullRequestSnapshot,
    /// Absent on hosts where native membership cannot be read.
    pub stack_number: Option<Option<u64>>,
}

/// A pending comment's id and the revision its lines now read at, if they still do.
pub type Moved = (u64, Option<String>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchoring {
    InDiff,
    OutsideDiff,
    /// The pull request is at another head now.
    Moved,
}

/// What became of a stack merge's submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeSubmission {
    /// Answered at once, or refused before anything was sent.
    Done(Outcome),
    /// The host works on operation `id`; `adopted` when it was already running.
    Following {
        id: String,
        adopted: bool,
        layers: Vec<u64>,
    },
}

/// A rebase of a native stack the host may start: its unmerged layers, bottom first, by number
/// and branch, and the run that rebases and pushes them.
pub struct StackRebase {
    pub layers: Vec<(u64, String)>,
    run: Run,
}
/// Rebases and pushes the layers, telling its argument each layer's step as it happens.
type Run = Box<dyn FnOnce(&mut dyn FnMut(usize, StackRebaseStep)) -> Outcome + Send>;
impl StackRebase {
    pub fn new(
        layers: Vec<(u64, String)>,
        run: impl FnOnce(&mut dyn FnMut(usize, StackRebaseStep)) -> Outcome + Send + 'static,
    ) -> Self {
        Self {
            layers,
            run: Box::new(run),
        }
    }

    /// Runs the rebase, telling `progress` each layer's step, and answers how it ended.
    pub fn run(self, mut progress: impl FnMut(usize, StackRebaseStep)) -> Outcome {
        (self.run)(&mut progress)
    }
}

/// Two parts, so a watch reads only what moved: `status` needs the detail read, `remarks` the
/// far costlier activity read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub status: String,
    pub remarks: String,
}

/// Replies past one review thread's first page.
#[derive(Debug, Clone)]
pub struct Tail {
    pub(crate) count: u64,
    pub(crate) comments: Vec<PullRequestRemark>,
}
pub type Tails = HashMap<String, Tail>;
