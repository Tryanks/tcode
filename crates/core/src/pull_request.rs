use serde::{Deserialize, Serialize};

/// Prepended to each turn while the pull request tools are registered; the transcript shows it.
pub const LINKING_INSTRUCTIONS: &str = "<pull_request_linking>\nWhen the tcode_pull_requests MCP server exposes link_pull_request, use it to register every pull request you create or work on for this thread. Call link_pull_request with the full PR URL immediately after creating a PR or starting work on an existing PR. For a stack, link every layer, not just the current branch or top PR. This applies to gh, gh stack, other CLIs and host APIs: they do not register PRs with this thread. Linking an already-linked PR is safe. Before finishing PR work, call list_thread_pull_requests and link anything missing. Do not link unrelated PRs mentioned only as background. If linking fails, report that failure instead of claiming the PR is linked.\nWhen asked to monitor, watch, or babysit a PR and watch_pull_request is available, call it and end your turn: Tcode wakes you when checks finish, someone else comments, or the branch conflicts, so do not poll or run your own watcher. A wake is news, not a merge decision: check readiness yourself before merging. When you hand the work back to the user, call unwatch_pull_request first.\nFor dependent changes, GitHub native stacks preserve the full bottom-to-top topology and merge scope; see https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/working-with-stacked-pull-requests .\n</pull_request_linking>\n\n";

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
