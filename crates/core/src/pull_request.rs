use serde::{Deserialize, Serialize};

macro_rules! stacks_docs {
    () => {
        "https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/working-with-stacked-pull-requests"
    };
}

/// GitHub's documentation of native stacks, which the instructions and the stack map both link.
pub const STACKS_DOCS_URL: &str = stacks_docs!();

/// Prepended to each turn while the pull request tools are registered; the transcript shows it.
pub const LINKING_INSTRUCTIONS: &str = concat!(
    "<pull_request_linking>\nWhen the tcode_pull_requests MCP server exposes link_pull_request, use it to register every pull request you create or work on for this thread. Call link_pull_request with the full PR URL immediately after creating a PR or starting work on an existing PR. For a stack, link every layer, not just the current branch or top PR. This applies to gh, gh stack, other CLIs and host APIs: they do not register PRs with this thread. Linking an already-linked PR is safe. Before finishing PR work, call list_thread_pull_requests and link anything missing. Do not link unrelated PRs mentioned only as background. If linking fails, report that failure instead of claiming the PR is linked.\nWhen asked to monitor, watch, or babysit a PR and watch_pull_request is available, call it and end your turn: Tcode wakes you when checks finish, someone else comments, or the branch conflicts, so do not poll or run your own watcher. A wake is news, not a merge decision: check readiness yourself before merging. When you hand the work back to the user, call unwatch_pull_request first.\nFor dependent changes, GitHub native stacks preserve the full bottom-to-top topology and merge scope; see ",
    stacks_docs!(),
    " .\n</pull_request_linking>\n\n"
);

/// The rest of a turn's injected context when it leads with [`LINKING_INSTRUCTIONS`].
pub fn strip_linking_instructions(context: &str) -> Option<&str> {
    context.strip_prefix(LINKING_INSTRUCTIONS)
}

/// `repository` is a canonical locator supplied by the forge adapter, opaque to core.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PullRequestKey {
    #[serde(deserialize_with = "canonical_locator")]
    pub host: String,
    #[serde(deserialize_with = "canonical_locator")]
    pub repository: String,
    pub number: u64,
}

fn canonical_locator<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<String, D::Error> {
    Ok(String::deserialize(decoder)?.trim().to_ascii_lowercase())
}

impl PullRequestKey {
    pub fn new(host: &str, repository: &str, number: u64) -> Self {
        Self {
            host: host.trim().to_ascii_lowercase(),
            repository: repository.trim().to_ascii_lowercase(),
            number,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestSource {
    Manual,
    Created,
    Agent,
    Stack,
    /// A key-only tombstone left by every unlink, so sync and discovery never relink it.
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestAuthor {
    pub login: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Passing,
    Failing,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    Required,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Clean,
    Conflicting,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestSnapshot {
    pub state: PullRequestState,
    pub title: String,
    pub head_branch: String,
    pub base_branch: String,
    pub is_draft: bool,
    pub updated_at: String,
    pub synced_at: u64,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub author: Option<PullRequestAuthor>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub review_decision: Option<ReviewDecision>,
    pub checks_state: Option<ChecksState>,
    #[serde(default)]
    pub mergeability: Mergeability,
}

impl PullRequestSnapshot {
    pub fn same_observation(&self, other: &Self) -> bool {
        let mut observation = other.clone();
        observation.synced_at = self.synced_at;
        self == &observation
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestStackLayer {
    pub url: String,
    pub number: u64,
    pub head_branch: String,
    pub state: PullRequestState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestStack {
    pub id: String,
    pub number: u64,
    pub url: String,
    pub base: String,
    /// The forge supplies the full topology, bottom to top, independently of links.
    pub layers: Vec<PullRequestStackLayer>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "stack", rename_all = "snake_case")]
pub enum PullRequestStackState {
    #[default]
    Unknown,
    None,
    Native(PullRequestStack),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum PullRequestSyncError {
    NoCredential,
    HostDisabled,
    RateLimited { retry_at: u64 },
    NotFound,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadPullRequestLink {
    pub key: PullRequestKey,
    pub source: PullRequestSource,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<PullRequestSnapshot>,
    #[serde(default, skip_serializing_if = "is_unknown")]
    pub stack: PullRequestStackState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_error: Option<PullRequestSyncError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<crate::pull_request_watch::PullRequestWatch>,
}

fn is_unknown(stack: &PullRequestStackState) -> bool {
    *stack == PullRequestStackState::Unknown
}

impl ThreadPullRequestLink {
    pub fn visible(&self) -> bool {
        self.source != PullRequestSource::Dismissed
    }
}

/// Whether the thread shows the pull request: a visible link, or a layer of the native stack
/// a visible link carries.
pub fn shown(links: &[ThreadPullRequestLink], key: &PullRequestKey) -> bool {
    links.iter().filter(|link| link.visible()).any(|link| {
        link.key == *key
            || matches!(&link.stack, PullRequestStackState::Native(stack)
                if link.key.host == key.host
                    && link.key.repository == key.repository
                    && stack.layers.iter().any(|layer| layer.number == key.number))
    })
}

/// GitHub's merge methods, in the order its merge button lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestMergeMethod {
    Merge,
    Squash,
    Rebase,
}

/// How a merge, a branch update or auto-merge may reach a pull request the thread shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestStackRoute {
    /// In no native stack: it merges as one pull request.
    Single,
    /// Layer `index` (from 1, bottom first) of a native stack of `layers`, which GitHub merges
    /// with the layers below it.
    Layer { index: u32, layers: u32 },
    /// Whether it is in a native stack is not known yet.
    Unknown,
}

pub fn stack_route(links: &[ThreadPullRequestLink], key: &PullRequestKey) -> PullRequestStackRoute {
    let visible = || links.iter().filter(|link| link.visible());
    let layer = visible().find_map(|link| match &link.stack {
        PullRequestStackState::Native(stack)
            if link.key.host == key.host && link.key.repository == key.repository =>
        {
            let index = stack
                .layers
                .iter()
                .position(|layer| layer.number == key.number)?;
            Some(PullRequestStackRoute::Layer {
                index: index as u32 + 1,
                layers: stack.layers.len() as u32,
            })
        }
        _ => None,
    });
    match (layer, visible().find(|link| link.key == *key)) {
        (Some(layer), _) => layer,
        (None, Some(link)) if link.stack == PullRequestStackState::None => {
            PullRequestStackRoute::Single
        }
        _ => PullRequestStackRoute::Unknown,
    }
}

/// The native stack a visible link of the thread carries with `key` among its layers.
pub fn native_stack<'a>(
    links: &'a [ThreadPullRequestLink],
    key: &PullRequestKey,
) -> Option<&'a PullRequestStack> {
    links
        .iter()
        .filter(|link| link.visible())
        .find_map(|link| match &link.stack {
            PullRequestStackState::Native(stack)
                if link.key.host == key.host
                    && link.key.repository == key.repository
                    && stack.layers.iter().any(|layer| layer.number == key.number) =>
            {
                Some(stack)
            }
            _ => None,
        })
}

/// Why GitHub will not merge a layer the merge scope holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackBlocker {
    Draft,
    Closed,
}

/// How the thread holds a layer of its stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackLayerCondition {
    Linked,
    Dismissed,
    NotLinked,
}

/// What a layer is to a merge of the selected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackLayerRole {
    Selected,
    /// Merged already, below the selected layer: not part of the merge.
    BelowMerged,
    /// Lands together with the selected layer.
    InScope,
    /// In the scope, and GitHub refuses to merge it.
    Blocks(StackBlocker),
    Above,
}

#[derive(Debug, Clone)]
pub struct StackMapRow<'a> {
    pub layer: &'a PullRequestStackLayer,
    /// The thread's visible link of the layer, whose snapshot titles it.
    pub link: Option<&'a ThreadPullRequestLink>,
    /// `None` where no snapshot of the layer says.
    pub draft: Option<bool>,
    pub condition: StackLayerCondition,
    pub role: StackLayerRole,
}

/// A native stack from the selected layer's point of view: every layer bottom to top, the
/// layers a merge of the selected one lands, and those GitHub refuses among them. Read from the
/// stored topology and links only; a write reads GitHub again.
#[derive(Debug, Clone)]
pub struct StackMap<'a> {
    pub stack: &'a PullRequestStack,
    pub rows: Vec<StackMapRow<'a>>,
    pub selected: usize,
}

impl StackMap<'_> {
    pub fn selected(&self) -> &StackMapRow<'_> {
        &self.rows[self.selected]
    }

    /// The unmerged layers from the bottom through the selected one, bottom first: what Merge
    /// stack lands.
    pub fn scope(&self) -> Vec<u64> {
        self.rows[..=self.selected]
            .iter()
            .filter(|row| row.layer.state != PullRequestState::Merged)
            .map(|row| row.layer.number)
            .collect()
    }

    /// The scope's layers GitHub refuses, below the selected one, bottom first.
    pub fn blockers(&self) -> Vec<(u64, StackBlocker)> {
        self.rows
            .iter()
            .filter_map(|row| match row.role {
                StackLayerRole::Blocks(blocker) => Some((row.layer.number, blocker)),
                _ => None,
            })
            .collect()
    }

    /// The open layers a rebase moves, bottom first.
    pub fn unmerged(&self) -> Vec<u64> {
        self.rows
            .iter()
            .filter(|row| row.layer.state != PullRequestState::Merged)
            .map(|row| row.layer.number)
            .collect()
    }
}

/// The selected layer's map of the native stack the thread shows it in. Every layer being
/// merged must be open and ready for review: a closed or draft layer below the selected one
/// blocks it, and one whose draft flag is unknown does not until a fresh read says.
pub fn stack_map<'a>(
    links: &'a [ThreadPullRequestLink],
    key: &PullRequestKey,
) -> Option<StackMap<'a>> {
    let stack = native_stack(links, key)?;
    let selected = stack
        .layers
        .iter()
        .position(|layer| layer.number == key.number)?;
    let rows = stack
        .layers
        .iter()
        .enumerate()
        .map(|(index, layer)| {
            let held = links.iter().find(|link| {
                link.key.host == key.host
                    && link.key.repository == key.repository
                    && link.key.number == layer.number
            });
            let link = held.filter(|link| link.visible());
            let condition = match held {
                Some(link) if link.visible() => StackLayerCondition::Linked,
                Some(_) => StackLayerCondition::Dismissed,
                None => StackLayerCondition::NotLinked,
            };
            let draft = link
                .and_then(|link| link.snapshot.as_ref())
                .map(|snapshot| snapshot.is_draft);
            let role = if index == selected {
                StackLayerRole::Selected
            } else if index > selected {
                StackLayerRole::Above
            } else if layer.state == PullRequestState::Merged {
                StackLayerRole::BelowMerged
            } else if layer.state == PullRequestState::Closed {
                StackLayerRole::Blocks(StackBlocker::Closed)
            } else if draft == Some(true) {
                StackLayerRole::Blocks(StackBlocker::Draft)
            } else {
                StackLayerRole::InScope
            };
            StackMapRow {
                layer,
                link,
                draft,
                condition,
                role,
            }
        })
        .collect();
    Some(StackMap {
        stack,
        rows,
        selected,
    })
}

/// A write to a native stack that the host is running, or whose end it could not confirm. The
/// host publishes it with every thread that shows the stack, so each device and a reconnecting
/// one show the same state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestStackOperation {
    #[serde(deserialize_with = "canonical_locator")]
    pub host: String,
    #[serde(deserialize_with = "canonical_locator")]
    pub repository: String,
    pub stack: u64,
    pub started_at: u64,
    pub kind: StackOperationKind,
}

impl PullRequestStackOperation {
    pub fn is_for(&self, host: &str, repository: &str, stack: u64) -> bool {
        self.host == host && self.repository == repository && self.stack == stack
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum StackOperationKind {
    /// GitHub's asynchronous merge `id`, submitted from `target`, landing `layers` bottom first.
    /// `adopted` when GitHub named an operation already running instead of taking a new one.
    Merging {
        id: String,
        target: u64,
        layers: Vec<u64>,
        adopted: bool,
    },
    /// Still pending when the host stopped following it; never submitted again. `checked` once a
    /// sync after that read the target still open, so the stack's writes are offered again.
    MergeUnconfirmed {
        id: String,
        target: u64,
        layers: Vec<u64>,
        checked: bool,
    },
    Rebasing {
        layers: Vec<StackRebaseLayer>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackRebaseLayer {
    pub number: u64,
    pub branch: String,
    pub step: StackRebaseStep,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum StackRebaseStep {
    Waiting,
    Rebasing,
    Pushing,
    Pushed { from: String, to: String },
    AlreadyCurrent,
    Failed { reason: StackRebaseFailure },
    /// Above the layer the rebase stopped at.
    NotStarted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum StackRebaseFailure {
    /// The layer's own commits do not apply onto the layer below; the scratch rebase was
    /// aborted and the branch left as it was.
    Conflict,
    /// The branch is no longer at the head that was reviewed, so the lease kept it.
    LeaseRefused,
    /// The push got no answer in time, so it may have landed.
    PushUnconfirmed,
    Git {
        step: StackRebaseGitStep,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackRebaseGitStep {
    Preparing,
    Fetching,
    ForkPoint,
    CheckingOut,
    Rebasing,
    Pushing,
}

/// The operation the thread shows for the native stack `key` is a layer of.
pub fn stack_operation<'a>(
    operations: &'a [PullRequestStackOperation],
    links: &[ThreadPullRequestLink],
    key: &PullRequestKey,
) -> Option<&'a PullRequestStackOperation> {
    let stack = native_stack(links, key)?;
    operations
        .iter()
        .find(|operation| operation.is_for(&key.host, &key.repository, stack.number))
}

/// The visible links a watch holds, which keep their thread waiting between wakes.
pub fn watched(links: &[ThreadPullRequestLink]) -> impl Iterator<Item = &ThreadPullRequestLink> {
    links
        .iter()
        .filter(|link| link.visible() && link.watch.is_some())
}

pub fn link_pull_request(
    links: &mut Vec<ThreadPullRequestLink>,
    key: PullRequestKey,
    url: String,
    source: PullRequestSource,
    now: u64,
    explicit: bool,
) -> bool {
    if let Some(existing) = links.iter_mut().find(|link| link.key == key) {
        if existing.visible() || !explicit {
            return false;
        }
        *existing = ThreadPullRequestLink {
            key,
            url,
            source,
            linked_at: Some(now),
            snapshot: None,
            stack: PullRequestStackState::Unknown,
            sync_error: None,
            watch: None,
        };
    } else {
        links.push(ThreadPullRequestLink {
            key,
            url,
            source,
            linked_at: Some(now),
            snapshot: None,
            stack: PullRequestStackState::Unknown,
            sync_error: None,
            watch: None,
        });
    }
    true
}

/// Leaves a key-only tombstone, which only an explicit link restores.
pub fn unlink_pull_request(links: &mut [ThreadPullRequestLink], key: &PullRequestKey) -> bool {
    let Some(link) = links
        .iter_mut()
        .find(|link| &link.key == key && link.visible())
    else {
        return false;
    };
    *link = ThreadPullRequestLink {
        key: key.clone(),
        source: PullRequestSource::Dismissed,
        url: String::new(),
        linked_at: None,
        snapshot: None,
        stack: PullRequestStackState::Unknown,
        sync_error: None,
        watch: None,
    };
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestBadgeState {
    Draft,
    Unknown,
    Open,
    Merged,
    Closed,
}

pub fn badge(links: &[ThreadPullRequestLink]) -> Option<(PullRequestBadgeState, usize, bool)> {
    let visible: Vec<_> = links.iter().filter(|link| link.visible()).collect();
    if visible.is_empty() {
        return None;
    }
    let open: Vec<_> = visible
        .iter()
        .filter(|link| {
            link.snapshot
                .as_ref()
                .is_none_or(|s| s.state == PullRequestState::Open)
        })
        .collect();
    let state = if visible.iter().all(|link| link.snapshot.is_none()) {
        PullRequestBadgeState::Unknown
    } else if !open.is_empty() {
        if visible.iter().all(|link| {
            link.snapshot
                .as_ref()
                .is_some_and(|s| s.state == PullRequestState::Open && s.is_draft)
        }) {
            PullRequestBadgeState::Draft
        } else {
            PullRequestBadgeState::Open
        }
    } else if visible.iter().all(|link| {
        link.snapshot
            .as_ref()
            .is_some_and(|s| s.state == PullRequestState::Merged)
    }) {
        PullRequestBadgeState::Merged
    } else {
        PullRequestBadgeState::Closed
    };
    let chains = groups(links);
    if visible.len() > 1 && chains.len() == 1 {
        return Some((state, visible.len(), true));
    }
    Some((state, visible.len(), false))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestGroupKind {
    Native,
    Derived,
    Single,
}
#[derive(Debug, Clone)]
pub struct PullRequestGroup<'a> {
    pub kind: PullRequestGroupKind,
    pub stack: Option<&'a PullRequestStack>,
    pub links: Vec<&'a ThreadPullRequestLink>,
}

pub fn groups(links: &[ThreadPullRequestLink]) -> Vec<PullRequestGroup<'_>> {
    use std::collections::{HashMap, HashSet};
    let visible: Vec<_> = links.iter().filter(|link| link.visible()).collect();
    let mut groups = Vec::new();
    let mut placed = HashSet::new();
    for link in &visible {
        let PullRequestStackState::Native(stack) = &link.stack else {
            continue;
        };
        if placed.contains(&link.key) {
            continue;
        }
        let mut members: Vec<_> = stack
            .layers
            .iter()
            .filter_map(|layer| {
                visible
                    .iter()
                    .find(|member| {
                        member.key.host == link.key.host
                            && member.key.repository == link.key.repository
                            && member.key.number == layer.number
                    })
                    .copied()
            })
            .collect();
        if !members.iter().any(|member| member.key == link.key) {
            members.push(link);
        }
        placed.extend(members.iter().map(|member| member.key.clone()));
        groups.push(PullRequestGroup {
            kind: PullRequestGroupKind::Native,
            stack: Some(stack),
            links: members,
        });
    }
    let remaining: Vec<_> = visible
        .iter()
        .filter(|link| !placed.contains(&link.key))
        .copied()
        .collect();
    let mut by_head: HashMap<(&str, &str, &str), Option<&ThreadPullRequestLink>> = HashMap::new();
    for link in &remaining {
        if let Some(snapshot) = &link.snapshot {
            let key = (
                link.key.host.as_str(),
                link.key.repository.as_str(),
                snapshot.head_branch.as_str(),
            );
            if let std::collections::hash_map::Entry::Vacant(entry) = by_head.entry(key) {
                entry.insert(Some(*link));
            } else {
                by_head.insert(key, None);
            }
        }
    }
    let parent = |link: &ThreadPullRequestLink| {
        link.snapshot
            .as_ref()
            .and_then(|snapshot| {
                by_head
                    .get(&(
                        link.key.host.as_str(),
                        link.key.repository.as_str(),
                        snapshot.base_branch.as_str(),
                    ))
                    .copied()
                    .flatten()
            })
            .filter(|parent| parent.key != link.key)
    };
    let parents: HashSet<_> = remaining
        .iter()
        .filter_map(|link| parent(link).map(|link| &link.key))
        .collect();
    for top in &remaining {
        if parents.contains(&top.key) {
            continue;
        }
        let mut members = Vec::new();
        let mut cursor = Some(*top);
        while let Some(link) = cursor {
            if !placed.insert(link.key.clone()) {
                break;
            }
            members.insert(0, link);
            cursor = parent(link);
        }
        if !members.is_empty() {
            groups.push(PullRequestGroup {
                kind: if members.len() > 1 {
                    PullRequestGroupKind::Derived
                } else {
                    PullRequestGroupKind::Single
                },
                stack: None,
                links: members,
            });
        }
    }
    for link in remaining {
        if placed.insert(link.key.clone()) {
            groups.push(PullRequestGroup {
                kind: PullRequestGroupKind::Single,
                stack: None,
                links: vec![link],
            });
        }
    }
    groups.sort_by_key(|group| {
        std::cmp::Reverse(
            group
                .links
                .iter()
                .map(|link| {
                    link.snapshot
                        .as_ref()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s.updated_at).ok())
                        .map(|time| time.timestamp().max(0) as u64)
                        .unwrap_or_else(|| link.linked_at.unwrap_or(0))
                })
                .max(),
        )
    });
    groups
}

/// A review being written on the host. GitHub sees none of it until it is submitted whole, so a
/// draft outlives a client, a device and a moved head alike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewDraft {
    pub key: PullRequestKey,
    /// The GitHub account writing it, as the conversation names it: a draft is another
    /// account's work once the host reads as someone else, so it is kept and not shown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account: String,
    /// The head commit the comments are anchored at.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub head: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<PullRequestReviewDraftComment>,
    /// Never reused, so an edit naming a removed comment cannot reach a later one.
    #[serde(default)]
    pub next_id: u64,
    /// The last submission got no answer, so it may have been posted; set until the pull request
    /// is read again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub uncertain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewDraftComment {
    pub id: u64,
    /// The commit whose text the comment's side shows: the base for the old side, the head for
    /// the new.
    pub revision: String,
    pub path: String,
    pub side: crate::session::ReviewSide,
    pub start_line: u32,
    pub end_line: u32,
    pub body: String,
    /// False once the head moved and its lines no longer read as they did: kept, never sent.
    #[serde(default = "placed", skip_serializing_if = "is_placed")]
    pub placed: bool,
}

fn placed() -> bool {
    true
}

fn is_placed(placed: &bool) -> bool {
    *placed
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestReviewDraftEdit {
    /// Lines of the diff read at `head`; a draft anchored at another head takes no new comments
    /// until it is moved to this one.
    AddComment {
        head: String,
        revision: String,
        path: String,
        side: crate::session::ReviewSide,
        start_line: u32,
        end_line: u32,
        body: String,
    },
    EditComment {
        id: u64,
        body: String,
    },
    RemoveComment {
        id: u64,
    },
    SetBody {
        body: String,
    },
    /// Re-anchors the comments at the pull request's current head. The host applies it with
    /// [`reanchor_review`], since only it can read whether each comment's lines changed.
    MoveToHead,
    Discard,
}

/// The account's draft of the pull request.
pub fn review_draft<'a>(
    drafts: &'a [PullRequestReviewDraft],
    key: &PullRequestKey,
    account: &str,
) -> Option<&'a PullRequestReviewDraft> {
    drafts
        .iter()
        .find(|draft| draft.key == *key && draft.account == account)
}

fn draft_index(
    drafts: &[PullRequestReviewDraft],
    key: &PullRequestKey,
    account: &str,
) -> Option<usize> {
    drafts
        .iter()
        .position(|draft| draft.key == *key && draft.account == account)
}

/// Applies an edit to the account's draft; false when it changed nothing, was malformed, or was
/// [`PullRequestReviewDraftEdit::MoveToHead`].
pub fn edit_review_draft(
    drafts: &mut Vec<PullRequestReviewDraft>,
    key: &PullRequestKey,
    account: &str,
    edit: PullRequestReviewDraftEdit,
) -> bool {
    let index = match draft_index(drafts, key, account) {
        Some(index) => index,
        None if matches!(edit, PullRequestReviewDraftEdit::AddComment { .. })
            || matches!(&edit, PullRequestReviewDraftEdit::SetBody { body } if !body.is_empty()) =>
        {
            drafts.push(PullRequestReviewDraft {
                key: key.clone(),
                account: account.to_owned(),
                head: String::new(),
                body: String::new(),
                comments: Vec::new(),
                next_id: 1,
                uncertain: false,
            });
            drafts.len() - 1
        }
        None => return false,
    };
    let draft = &mut drafts[index];
    let changed = match edit {
        PullRequestReviewDraftEdit::AddComment {
            head,
            revision,
            path,
            side,
            start_line,
            end_line,
            body,
        } => {
            let anchored = draft.comments.is_empty() || draft.head == head;
            if !anchored
                || head.is_empty()
                || path.is_empty()
                || start_line == 0
                || start_line > end_line
                || body.trim().is_empty()
            {
                false
            } else {
                draft.head = head;
                draft.comments.push(PullRequestReviewDraftComment {
                    id: draft.next_id,
                    revision,
                    path,
                    side,
                    start_line,
                    end_line,
                    body,
                    placed: true,
                });
                draft.next_id += 1;
                true
            }
        }
        PullRequestReviewDraftEdit::EditComment { id, body } => {
            match draft.comments.iter_mut().find(|comment| comment.id == id) {
                Some(comment) if !body.trim().is_empty() && comment.body != body => {
                    comment.body = body;
                    true
                }
                _ => false,
            }
        }
        PullRequestReviewDraftEdit::RemoveComment { id } => {
            let before = draft.comments.len();
            draft.comments.retain(|comment| comment.id != id);
            draft.comments.len() != before
        }
        PullRequestReviewDraftEdit::SetBody { body } => {
            body != std::mem::replace(&mut draft.body, body.clone())
        }
        PullRequestReviewDraftEdit::MoveToHead => false,
        PullRequestReviewDraftEdit::Discard => {
            drafts.remove(index);
            return true;
        }
    };
    if draft.body.is_empty() && draft.comments.is_empty() {
        drafts.remove(index);
    }
    changed
}

/// Anchors the draft at `head`. `moved` names where each comment's lines are now, or `None` when
/// they changed, which leaves the comment unplaced.
pub fn reanchor_review(
    drafts: &mut [PullRequestReviewDraft],
    key: &PullRequestKey,
    account: &str,
    head: &str,
    moved: impl Fn(&PullRequestReviewDraftComment) -> Option<String>,
) -> bool {
    let Some(index) = draft_index(drafts, key, account) else {
        return false;
    };
    let draft = &mut drafts[index];
    if draft.head == head {
        return false;
    }
    draft.head = head.to_owned();
    for comment in &mut draft.comments {
        match moved(comment) {
            Some(revision) if comment.placed => comment.revision = revision,
            _ => comment.placed = false,
        }
    }
    true
}

/// What a submission did to the draft. Once GitHub took it, its comments go by id, and its body
/// only while it still reads as sent, since a body revised meanwhile is new work. An unanswered
/// one marks the draft until the pull request is read again.
pub fn submitted_review(
    drafts: &mut Vec<PullRequestReviewDraft>,
    key: &PullRequestKey,
    account: &str,
    sent: Option<(&[u64], &str)>,
) -> bool {
    let Some(index) = draft_index(drafts, key, account) else {
        return false;
    };
    let draft = &mut drafts[index];
    let Some((comments, body)) = sent else {
        draft.uncertain = true;
        return true;
    };
    draft
        .comments
        .retain(|comment| !comments.contains(&comment.id));
    if draft.body == body {
        draft.body.clear();
    }
    draft.uncertain = false;
    if draft.body.is_empty() && draft.comments.is_empty() {
        drafts.remove(index);
    }
    true
}

/// A menu visibility hint only; the host forge adapter validates and canonicalizes the target.
pub fn is_pull_request_url(value: &str) -> bool {
    let value = value.split(['?', '#']).next().unwrap_or_default();
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    let parts: Vec<_> = rest.split('/').collect();
    let [host, owner, repository, "pull", number, ..] = parts.as_slice() else {
        return false;
    };
    !host.is_empty()
        && !owner.is_empty()
        && !repository.is_empty()
        && number.parse::<u64>().is_ok_and(|number| number > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn link(number: u64, state: Option<PullRequestState>, draft: bool) -> ThreadPullRequestLink {
        ThreadPullRequestLink {
            key: PullRequestKey::new("github.com", "sample/project", number),
            url: format!("https://github.com/sample/project/pull/{number}"),
            source: PullRequestSource::Manual,
            linked_at: Some(number),
            snapshot: state.map(|state| PullRequestSnapshot {
                state,
                title: "Change".into(),
                head_branch: format!("layer-{number}"),
                base_branch: "main".into(),
                is_draft: draft,
                updated_at: "2026-10-08T00:00:00Z".into(),
                synced_at: 1,
                closed_at: None,
                merged_at: None,
                author: None,
                additions: 0,
                deletions: 0,
                changed_files: 0,
                review_decision: None,
                checks_state: None,
                mergeability: Mergeability::Unknown,
            }),
            stack: PullRequestStackState::Unknown,
            sync_error: None,
            watch: None,
        }
    }
    #[test]
    fn badge_distinguishes_unknown_drafts_terminal_and_related_links() {
        use PullRequestBadgeState as Badge;
        use PullRequestState as State;
        let cases = [
            (vec![], None),
            (vec![link(1, None, false)], Some((Badge::Unknown, 1, false))),
            (
                vec![
                    link(1, Some(State::Merged), false),
                    link(2, Some(State::Open), true),
                ],
                Some((Badge::Open, 2, false)),
            ),
            (
                vec![
                    link(1, Some(State::Open), true),
                    link(2, Some(State::Open), true),
                ],
                Some((Badge::Draft, 2, false)),
            ),
            (
                vec![link(1, Some(State::Merged), false), link(2, None, false)],
                Some((Badge::Open, 2, false)),
            ),
            (
                vec![
                    link(1, Some(State::Merged), false),
                    link(2, Some(State::Merged), false),
                ],
                Some((Badge::Merged, 2, false)),
            ),
            (
                vec![
                    link(1, Some(State::Merged), false),
                    link(2, Some(State::Closed), false),
                ],
                Some((Badge::Closed, 2, false)),
            ),
        ];
        for (links, expected) in cases {
            assert_eq!(badge(&links), expected);
        }
        let mut links = vec![
            link(1, Some(State::Open), false),
            link(2, Some(State::Open), false),
            link(3, Some(State::Open), false),
        ];
        links[1].snapshot.as_mut().unwrap().base_branch = "layer-1".into();
        links[2].snapshot.as_mut().unwrap().base_branch = "layer-2".into();
        assert_eq!(badge(&links), Some((Badge::Open, 3, true)));
        assert_eq!(
            groups(&links)[0]
                .links
                .iter()
                .map(|link| link.key.number)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        links[1].source = PullRequestSource::Dismissed;
        assert_eq!(badge(&links), Some((Badge::Open, 2, false)));
        assert!(is_pull_request_url(
            "https://github.com/sample/project/pull/123/files"
        ));
        assert!(is_pull_request_url(
            "https://github.com/sample/project/pull/123?view=1#issuecomment-7"
        ));
        for ordinary in [
            "https://github.com/sample/project/issues/123",
            "https://github.com/sample/project/pull/0",
            "https://example.test",
            "#123",
        ] {
            assert!(!is_pull_request_url(ordinary));
        }
    }
}
