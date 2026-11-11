use super::{
    GitHubApi, GitHubError, RequestOptions, RestRequest,
    api::Authentication,
    graphql::{self, AliasItem, Variables},
    repository::Repository,
};
use crate::forge::Summary;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tcode_core::pull_request::{
    ChecksState, Mergeability, PullRequestAuthor, PullRequestKey, PullRequestSnapshot,
    PullRequestStack, PullRequestStackLayer, PullRequestStackState, PullRequestState,
    ReviewDecision,
};

struct Job<K, V> {
    key: K,
    reply: mpsc::Sender<Result<V, GitHubError>>,
}
struct Batcher<K, V> {
    pending: Mutex<HashMap<String, Vec<Job<K, V>>>>,
    ready: Condvar,
}
impl<K, V> Default for Batcher<K, V> {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            ready: Condvar::new(),
        }
    }
}
impl<K: Clone, V> Batcher<K, V> {
    fn read(
        &self,
        group: String,
        key: K,
        width: usize,
        delay: Duration,
        read: impl Fn(&[K]) -> Vec<Result<V, GitHubError>>,
    ) -> Result<V, GitHubError> {
        let (reply, receiver) = mpsc::channel();
        let mut pending = self.pending.lock().unwrap();
        let jobs = pending.entry(group.clone()).or_default();
        let leader = jobs.is_empty();
        jobs.push(Job { key, reply });
        if jobs.len() >= width {
            self.ready.notify_all();
        }
        if leader {
            let (mut pending, _) = self
                .ready
                .wait_timeout_while(pending, delay, |pending| {
                    pending.get(&group).is_some_and(|jobs| jobs.len() < width)
                })
                .unwrap();
            let jobs = pending.remove(&group).unwrap();
            drop(pending);
            for chunk in jobs.chunks(width) {
                let keys: Vec<_> = chunk.iter().map(|job| job.key.clone()).collect();
                for (job, result) in chunk.iter().zip(read(&keys)) {
                    let _ = job.reply.send(result);
                }
            }
        } else {
            drop(pending);
        }
        receiver.recv().map_err(|_| GitHubError::Request)?
    }
}

pub struct PullRequests {
    api: Arc<GitHubApi>,
    summaries: Batcher<PullRequestKey, Summary>,
}
impl PullRequests {
    pub fn new(api: Arc<GitHubApi>) -> Arc<Self> {
        Arc::new(Self {
            api,
            summaries: Batcher::default(),
        })
    }
    pub fn summary(&self, key: &PullRequestKey) -> Result<Summary, GitHubError> {
        let credential = self.api.credentials().get(&key.host)?;
        let group = format!("{}\0{}", key.host, credential.fingerprint);
        let options = RequestOptions {
            authentication: Authentication::Pinned(credential),
            operation: "PullRequestSummaries",
            ..Default::default()
        };
        self.summaries.read(
            group,
            key.clone(),
            25,
            Duration::from_millis(10),
            |keys| match self.read_summaries(keys, &options) {
                Ok(rows) => rows,
                Err(error) => keys.iter().map(|_| Err(error.clone())).collect(),
            },
        )
    }
    fn read_summaries(
        &self,
        keys: &[PullRequestKey],
        options: &RequestOptions,
    ) -> Result<Vec<Result<Summary, GitHubError>>, GitHubError> {
        let items: Option<Vec<_>> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let repository = Repository::from_key(key)?;
                Some(AliasItem {
                    key: index,
                    variables: Variables::from([
                        ("owner".into(), ("String!".into(), json!(repository.owner))),
                        ("name".into(), ("String!".into(), json!(repository.name))),
                        ("number".into(), ("Int!".into(), json!(key.number))),
                    ]),
                })
            })
            .collect();
        let host = &keys[0].host;
        let stacks = if host == "github.com" {
            " stack { number size baseRefName } stackEntry { position }"
        } else {
            ""
        };
        let selection = format!(
            "title state isDraft mergeable reviewDecision additions deletions changedFiles updatedAt mergedAt closedAt headRefName baseRefName author {{ login avatarUrl }} latestReviews(first: 20) {{ nodes {{ state author {{ login }} }} }} commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }}{stacks}"
        );
        let document = graphql::aliases(
            "query",
            "PullRequestSummaries",
            "s",
            &items.ok_or(GitHubError::InvalidInput)?,
            &Variables::new(),
            |v| {
                format!(
                    "repository(owner: {}, name: {}) {{ pullRequest(number: {}) {{ {selection} }} }}",
                    v["owner"], v["name"], v["number"]
                )
            },
            |fields| fields,
        )
        .ok_or(GitHubError::InvalidInput)?;
        let response: Value = self.api.graphql_partial(host, &document, options)?.json()?;
        Ok(summaries(&response, keys.len()))
    }
    pub fn stack(&self, key: &PullRequestKey) -> Result<PullRequestStackState, GitHubError> {
        if key.host != "github.com" {
            return Ok(PullRequestStackState::Unknown);
        }
        let repository = Repository::from_key(key).ok_or(GitHubError::InvalidInput)?;
        let path = format!(
            "/repos/{}/{}/stacks?pull_request={}",
            repository.owner, repository.name, key.number
        );
        let response = match self.api.rest(
            &key.host,
            RestRequest::get(&path),
            &RequestOptions {
                operation: "PullRequestStack",
                ..Default::default()
            },
        ) {
            Ok(response) => response,
            Err(GitHubError::NotFound) => return Ok(PullRequestStackState::None),
            Err(error) => return Err(error),
        };
        let raw: Value = response.json()?;
        let rows = raw.as_array().ok_or(GitHubError::InvalidResponse)?;
        let Some(stack) = rows.first() else {
            return Ok(PullRequestStackState::None);
        };
        decode_stack(stack, &repository)
            .map(PullRequestStackState::Native)
            .ok_or(GitHubError::InvalidResponse)
    }
}

fn string(raw: &Value, field: &str) -> Option<String> {
    raw[field].as_str().map(str::to_owned)
}
fn state(raw: &Value) -> Option<PullRequestState> {
    if raw
        .get("mergedAt")
        .or_else(|| raw.get("merged_at"))
        .is_some_and(|v| v.is_string())
    {
        return Some(PullRequestState::Merged);
    }
    match raw["state"].as_str()?.to_ascii_lowercase().as_str() {
        "open" => Some(PullRequestState::Open),
        "merged" => Some(PullRequestState::Merged),
        "closed" => Some(PullRequestState::Closed),
        _ => None,
    }
}
fn decode_summary(raw: &Value) -> Option<Summary> {
    let checks = raw["commits"]["nodes"]
        .as_array()
        .and_then(|rows| rows.last())
        .and_then(|row| row["commit"]["statusCheckRollup"]["state"].as_str())
        .and_then(|state| match state {
            "SUCCESS" => Some(ChecksState::Passing),
            "FAILURE" | "ERROR" => Some(ChecksState::Failing),
            "PENDING" | "EXPECTED" => Some(ChecksState::Pending),
            _ => None,
        });
    Some(Summary {
        snapshot: PullRequestSnapshot {
            state: state(raw)?,
            title: string(raw, "title")?,
            head_branch: string(raw, "headRefName")?,
            base_branch: string(raw, "baseRefName")?,
            is_draft: raw["isDraft"].as_bool().unwrap_or(false),
            updated_at: Some(string(raw, "updatedAt")?),
            synced_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            closed_at: string(raw, "closedAt"),
            merged_at: string(raw, "mergedAt"),
            author: string(&raw["author"], "login").map(|login| PullRequestAuthor {
                login,
                avatar_url: string(&raw["author"], "avatarUrl"),
            }),
            additions: raw["additions"].as_u64(),
            deletions: raw["deletions"].as_u64(),
            changed_files: raw["changedFiles"].as_u64(),
            review_decision: review_decision(raw),
            checks_state: checks,
            mergeability: match raw["mergeable"].as_str() {
                Some("MERGEABLE") => Mergeability::Clean,
                Some("CONFLICTING") => Mergeability::Conflicting,
                // GitHub's word for a computation it has not finished.
                Some("UNKNOWN") => Mergeability::Pending,
                _ => Mergeability::Unknown,
            },
        },
        stack_number: raw.get("stack").map(|stack| stack["number"].as_u64()),
    })
}
/// Each of `count` summaries a batched answer holds, alias `s{index}` for the index-th.
fn summaries(response: &Value, count: usize) -> Vec<Result<Summary, GitHubError>> {
    graphql::aliased(response, "s", count)
        .into_iter()
        .map(|data| decode_summary(&data?["pullRequest"]).ok_or(GitHubError::InvalidResponse))
        .collect()
}

pub(super) fn decode_stack(raw: &Value, repository: &Repository) -> Option<PullRequestStack> {
    let number = raw["number"].as_u64()?;
    let id = raw["id"].as_u64()?.to_string();
    let base = string(&raw["base"], "ref")?;
    let layers: Option<Vec<_>> = raw["pull_requests"]
        .as_array()?
        .iter()
        .map(|row| {
            Some(PullRequestStackLayer {
                url: repository.url(row["number"].as_u64()?),
                number: row["number"].as_u64()?,
                head_branch: string(&row["head"], "ref")?,
                state: state(row)?,
            })
        })
        .collect();
    Some(PullRequestStack {
        id,
        number,
        base,
        layers: layers?,
    })
}

/// GitHub's own decision; only where it gives none, as on a repository that requires no review,
/// what the latest reviews say.
fn review_decision(raw: &Value) -> Option<ReviewDecision> {
    match raw["reviewDecision"].as_str() {
        Some("APPROVED") => return Some(ReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => return Some(ReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => return Some(ReviewDecision::Required),
        _ => {}
    }
    let reviews = raw["latestReviews"]["nodes"].as_array();
    if reviews.is_some_and(|rows| rows.iter().any(|row| row["state"] == "CHANGES_REQUESTED")) {
        Some(ReviewDecision::ChangesRequested)
    } else if reviews.is_some_and(|rows| rows.iter().any(|row| row["state"] == "APPROVED")) {
        Some(ReviewDecision::Approved)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `PullRequest` node as GitHub's GraphQL API returns it for the summary selection. Shape:
    /// the `PullRequest` object and the `PullRequestState`, `MergeableState`,
    /// `PullRequestReviewDecision` and `StatusState` enums of GitHub's GraphQL reference
    /// (docs.github.com/en/graphql/reference/pulls and /commits). Content is invented.
    fn node() -> Value {
        json!({
            "title": "Teach the parser tabs",
            "state": "OPEN",
            "isDraft": false,
            "mergeable": "UNKNOWN",
            "reviewDecision": "REVIEW_REQUIRED",
            "additions": 12,
            "deletions": 3,
            "changedFiles": 2,
            "updatedAt": "2026-01-02T03:04:05Z",
            "mergedAt": null,
            "closedAt": null,
            "headRefName": "tabs",
            "baseRefName": "main",
            "author": { "login": "octo-writer", "avatarUrl": "https://avatars.example.test/u/1" },
            "latestReviews": { "nodes": [] },
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "state": "EXPECTED" } } }] }
        })
    }

    /// A batched answer in which some aliases failed. Shape: the GraphQL specification's
    /// response, `data` beside `errors` whose `path` names the failed field (spec.graphql.org,
    /// "Response", "Errors"), and GitHub's `type` on each error, `NOT_FOUND` for a repository
    /// the credential cannot see; each alias is the `repository` field the summary document
    /// asks for. Content is invented.
    #[test]
    fn a_batch_answers_each_pull_request_by_its_own_alias() {
        let response = json!({
            "data": {
                "s0": { "pullRequest": node() },
                "s1": null,
                "s2": { "pullRequest": null },
            },
            "errors": [
                { "type": "NOT_FOUND", "path": ["s1"],
                  "message": "Could not resolve to a Repository with the name 'example-org/gone'." },
                { "type": "FORBIDDEN", "path": ["s2", "pullRequest"],
                  "message": "Resource not accessible by integration" },
            ]
        });
        let read = summaries(&response, 3);
        assert_eq!(
            read[0]
                .as_ref()
                .map(|summary| summary.snapshot.title.as_str()),
            Ok("Teach the parser tabs")
        );
        assert_eq!(read[1].as_ref().err(), Some(&GitHubError::NotFound));
        assert_eq!(
            read[2].as_ref().err(),
            Some(&GitHubError::Response {
                status: 200,
                messages: vec!["Resource not accessible by integration".into()],
            })
        );
    }

    #[test]
    fn a_summary_is_what_github_answered_for_the_pull_request_asked_about() {
        let summary = decode_summary(&node()).expect("GitHub answered with a pull request");
        let snapshot = summary.snapshot;
        assert_eq!(snapshot.state, PullRequestState::Open);
        assert_eq!(snapshot.title, "Teach the parser tabs");
        assert_eq!(
            (snapshot.head_branch.as_str(), snapshot.base_branch.as_str()),
            ("tabs", "main")
        );
        assert_eq!(snapshot.mergeability, Mergeability::Pending);
        assert_eq!(snapshot.checks_state, Some(ChecksState::Pending));
        assert_eq!(
            (
                snapshot.additions,
                snapshot.deletions,
                snapshot.changed_files
            ),
            (Some(12), Some(3), Some(2))
        );
        assert_eq!(
            snapshot.author.map(|author| author.login).as_deref(),
            Some("octo-writer")
        );
        // Read without the stack fields, the node names no stack.
        assert_eq!(summary.stack_number, None);
    }

    /// `reviewDecision` is the pull request's status "with respect to code review"; an
    /// approval from someone whose review does not count leaves it `REVIEW_REQUIRED`.
    #[test]
    fn githubs_review_decision_stands_over_the_latest_reviews() {
        let decision = |decision: Value, reviews: &[&str]| {
            let mut node = node();
            node["reviewDecision"] = decision;
            node["latestReviews"]["nodes"] = reviews
                .iter()
                .map(|state| json!({ "state": state, "author": { "login": "octo-reviewer" } }))
                .collect();
            decode_summary(&node).unwrap().snapshot.review_decision
        };
        assert_eq!(
            decision(json!("REVIEW_REQUIRED"), &["APPROVED"]),
            Some(ReviewDecision::Required)
        );
        assert_eq!(
            decision(json!("APPROVED"), &["COMMENTED"]),
            Some(ReviewDecision::Approved)
        );
        // A repository that requires no review gets no decision from GitHub.
        assert_eq!(
            decision(Value::Null, &["APPROVED", "COMMENTED"]),
            Some(ReviewDecision::Approved)
        );
        assert_eq!(decision(Value::Null, &[]), None);
    }

    /// A stack as GitHub's REST API "Get a stack" returns it. Shape: the stack schema of
    /// docs.github.com/en/rest/pulls/stacks (`id` an integer, `base.ref`, each pull request's
    /// `number`, `state` open or closed, `merged_at`, `head.ref`). Content is invented.
    #[test]
    fn a_rest_stack_reads_its_layers_bottom_first() {
        let repository = Repository {
            host: "github.com".into(),
            owner: "example-org".into(),
            name: "tool".into(),
        };
        let stack = json!({
            "id": 4021,
            "number": 3,
            "node_id": "PRS_invented",
            "url": "https://api.github.com/repos/example-org/tool/stacks/3",
            "base": { "ref": "main" },
            "open": true,
            "created_at": "2026-01-02T03:04:05Z",
            "pull_requests": [
                { "number": 11, "state": "closed", "draft": false,
                  "merged_at": "2026-01-03T00:00:00Z",
                  "head": { "ref": "parser-1", "sha": "1111111111111111111111111111111111111111" } },
                { "number": 12, "state": "open", "draft": true, "merged_at": null,
                  "head": { "ref": "parser-2", "sha": "2222222222222222222222222222222222222222" } }
            ]
        });
        let stack = decode_stack(&stack, &repository).expect("a documented stack");
        assert_eq!((stack.id.as_str(), stack.number), ("4021", 3));
        assert_eq!(stack.base, "main");
        assert_eq!(
            stack
                .layers
                .iter()
                .map(|layer| (layer.number, layer.head_branch.as_str(), layer.state))
                .collect::<Vec<_>>(),
            [
                (11, "parser-1", PullRequestState::Merged),
                (12, "parser-2", PullRequestState::Open)
            ]
        );
        assert_eq!(
            stack.layers[1].url,
            "https://github.com/example-org/tool/pull/12"
        );
    }
}
