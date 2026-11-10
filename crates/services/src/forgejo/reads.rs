//! What Tcode reads of a Forgejo or Gitea pull request, in Tcode's terms.

use super::api::{Api, Request, Server};
use crate::forge::ForgeError;
use serde_json::Value;
use std::collections::BTreeMap;
use tcode_core::{
    pull_request::{
        ChecksState, Mergeability, PullRequestAuthor, PullRequestKey, PullRequestSnapshot,
        PullRequestState,
    },
    pull_request_watch::{CheckStatus, PullRequestCheck, PullRequestRemark},
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestActor, PullRequestCapabilities, PullRequestComment, PullRequestFile,
    PullRequestLabel, PullRequestReaction, PullRequestReactionContent, PullRequestReviewAnchor,
    PullRequestReviewState, PullRequestReviewThread, PullRequestReviewer, PullRequestReviewerKind,
    PullRequestReviewerState,
};

/// Past this many pages a list is reported incomplete rather than read on.
pub(super) const MAX_PAGES: usize = 10;
/// The pull request itself, as reactions and edits name it.
pub(super) const PULL_REQUEST: &str = "pr";
const REVIEW: &str = "review-";

pub(super) fn capabilities(server: Server) -> PullRequestCapabilities {
    PullRequestCapabilities {
        // Gitea's API replies to and resolves a review comment; Forgejo 17's has neither.
        reply: server.gitea,
        resolve: server.gitea,
        reactions: true,
        labels: true,
        request_changes: true,
        // Draft is a title prefix on these servers; no API field marks it.
        draft: false,
        reopen: true,
        // A merge can be scheduled for when checks pass, but nothing reads whether one is, so it
        // could never be shown or cancelled.
        auto_merge: false,
        update_branch: true,
        update_merge: true,
        revert: false,
        host_viewed_marks: false,
        // No read of the server's default merge message exists to send back cleaned.
        merge_message: false,
    }
}

/// A pull request's place on its server and the requests that read it.
pub(super) struct Pull<'a> {
    pub(super) api: &'a Api,
    pub(super) key: &'a PullRequestKey,
}

impl Pull<'_> {
    pub(super) fn authority(&self) -> &str {
        &self.key.host
    }
    pub(super) fn path(&self, rest: &str) -> String {
        format!("/repos/{}/{rest}", self.key.repository)
    }
    pub(super) fn get(&self, rest: &str, operation: &'static str) -> Result<Value, ForgeError> {
        self.api
            .send(self.authority(), Request::get(self.path(rest), operation))?
            .json()
    }
    pub(super) fn pull(&self) -> Result<Value, ForgeError> {
        self.get(&format!("pulls/{}", self.key.number), "PullRequest")
    }
    pub(super) fn repository(&self) -> Result<Value, ForgeError> {
        self.get("", "Repository")
    }
    /// The signed-in account, or `None` when reading anonymously.
    pub(super) fn viewer(&self) -> Result<Option<String>, ForgeError> {
        if self.api.credential(self.authority())?.is_none() {
            return Ok(None);
        }
        let user: Value = self
            .api
            .send(self.authority(), Request::get("/user", "Viewer"))?
            .json()?;
        Ok(user["login"].as_str().map(str::to_owned))
    }
    /// The newest status of each context on `sha`, as the server combines them. The servers
    /// ignore a sort on the plain list, which answers oldest first.
    pub(super) fn checks(&self, sha: &str) -> Result<Vec<PullRequestCheck>, ForgeError> {
        Ok(checks(&self.get(
            &format!("commits/{sha}/status?limit=50"),
            "CommitStatus",
        )?))
    }
    /// Every issue comment: the servers answer the whole list whatever page is asked for.
    pub(super) fn issue_comments(&self) -> Result<Vec<Value>, ForgeError> {
        Ok(self
            .get(
                &format!("issues/{}/comments", self.key.number),
                "IssueComments",
            )?
            .as_array()
            .cloned()
            .unwrap_or_default())
    }
}

/// The head branch: a merged pull request whose branch is gone names its pull ref instead, and
/// keeps the branch in its label.
pub(super) fn head_branch(pr: &Value) -> Option<String> {
    let head = &pr["head"];
    match text(head, "ref") {
        Some(reference) if reference.starts_with("refs/pull/") => text(head, "label")
            .map(|label| {
                label
                    .rsplit_once(':')
                    .map_or(label.clone(), |(_, branch)| branch.to_owned())
            })
            .or(Some(reference)),
        other => other,
    }
}

pub(super) fn text(raw: &Value, field: &str) -> Option<String> {
    raw[field].as_str().map(str::to_owned)
}

/// A combined status's `statuses`, one check each.
pub(super) fn checks(combined: &Value) -> Vec<PullRequestCheck> {
    combined["statuses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|status| {
            Some(PullRequestCheck {
                name: text(status, "context")?,
                status: check_status(status["status"].as_str().unwrap_or_default()),
                url: text(status, "target_url").filter(|url| !url.is_empty()),
                required: None,
            })
        })
        .collect()
}

/// A `warning` is a finished status asking for attention, so it fails rather than waits, as
/// Gitea's own combined state counts it.
fn check_status(state: &str) -> CheckStatus {
    match state {
        "success" => CheckStatus::Success,
        "failure" | "error" | "warning" => CheckStatus::Failure,
        "pending" => CheckStatus::Pending,
        "skipped" => CheckStatus::Skipped,
        _ => CheckStatus::Neutral,
    }
}

pub(super) fn checks_state(checks: &[PullRequestCheck]) -> Option<ChecksState> {
    if checks.is_empty() {
        None
    } else if checks.iter().any(|check| check.status.failed()) {
        Some(ChecksState::Failing)
    } else if checks
        .iter()
        .any(|check| check.status == CheckStatus::Pending)
    {
        Some(ChecksState::Pending)
    } else {
        Some(ChecksState::Passing)
    }
}

pub(super) fn state(pr: &Value) -> Option<PullRequestState> {
    if pr["merged"].as_bool() == Some(true) {
        return Some(PullRequestState::Merged);
    }
    match pr["state"].as_str()? {
        "open" => Some(PullRequestState::Open),
        "closed" => Some(PullRequestState::Closed),
        _ => None,
    }
}

/// What `mergeable` says: true is clean, and false is as much a conflict check still running or
/// failed, or a draft, as a conflict, and the API states nothing that tells them apart, so it
/// is unknown.
pub(super) fn mergeability(pr: &Value) -> Mergeability {
    if pr["mergeable"].as_bool() == Some(true) {
        Mergeability::Clean
    } else {
        Mergeability::Unknown
    }
}

pub(super) fn snapshot(
    pr: &Value,
    checks: Option<ChecksState>,
    mergeability: Mergeability,
    synced_at: u64,
) -> Option<PullRequestSnapshot> {
    Some(PullRequestSnapshot {
        state: state(pr)?,
        title: text(pr, "title")?,
        head_branch: head_branch(pr)?,
        base_branch: text(&pr["base"], "ref")?,
        is_draft: pr["draft"].as_bool().unwrap_or(false),
        updated_at: text(pr, "updated_at")?,
        synced_at,
        closed_at: text(pr, "closed_at"),
        merged_at: text(pr, "merged_at"),
        author: text(&pr["user"], "login").map(|login| PullRequestAuthor {
            login,
            avatar_url: text(&pr["user"], "avatar_url"),
        }),
        additions: Some(pr["additions"].as_u64().unwrap_or(0)),
        deletions: Some(pr["deletions"].as_u64().unwrap_or(0)),
        changed_files: Some(pr["changed_files"].as_u64().unwrap_or(0)),
        review_decision: None,
        checks_state: checks,
        mergeability,
    })
}

pub(super) fn actor(raw: &Value) -> Option<PullRequestActor> {
    Some(PullRequestActor {
        login: text(raw, "login")?,
        avatar_url: text(raw, "avatar_url"),
    })
}

/// When the text changed after it was written; the servers stamp `updated_at` on every save.
pub(super) fn edited_at(raw: &Value) -> Option<String> {
    let updated = text(raw, "updated_at")?;
    (Some(&updated) != raw["created_at"].as_str().map(str::to_owned).as_ref()
        && Some(&updated) != raw["submitted_at"].as_str().map(str::to_owned).as_ref())
    .then_some(updated)
}

pub(super) fn reaction_name(content: PullRequestReactionContent) -> &'static str {
    match content {
        PullRequestReactionContent::ThumbsUp => "+1",
        PullRequestReactionContent::ThumbsDown => "-1",
        PullRequestReactionContent::Laugh => "laugh",
        PullRequestReactionContent::Hooray => "hooray",
        PullRequestReactionContent::Confused => "confused",
        PullRequestReactionContent::Heart => "heart",
        PullRequestReactionContent::Rocket => "rocket",
        PullRequestReactionContent::Eyes => "eyes",
    }
}

pub(super) fn reactions(rows: &[Value], viewer: Option<&str>) -> Vec<PullRequestReaction> {
    PullRequestReactionContent::ALL
        .into_iter()
        .filter_map(|content| {
            let given: Vec<_> = rows
                .iter()
                .filter(|row| row["content"].as_str() == Some(reaction_name(content)))
                .collect();
            (!given.is_empty()).then(|| PullRequestReaction {
                content,
                count: given.len() as u64,
                viewer_reacted: viewer.is_some_and(|viewer| {
                    given
                        .iter()
                        .any(|row| row["user"]["login"].as_str() == Some(viewer))
                }),
            })
        })
        .collect()
}

pub(super) fn review_state(raw: &Value) -> Option<PullRequestReviewState> {
    if raw["dismissed"].as_bool() == Some(true) {
        return Some(PullRequestReviewState::Dismissed);
    }
    match raw["state"].as_str()? {
        "APPROVED" => Some(PullRequestReviewState::Approved),
        "REQUEST_CHANGES" => Some(PullRequestReviewState::ChangesRequested),
        "COMMENT" => Some(PullRequestReviewState::Commented),
        "PENDING" => Some(PullRequestReviewState::Pending),
        _ => None,
    }
}

/// A review's own body, under an id of its own: a review is not a comment that takes edits or
/// reactions here. A bodiless comment review only wraps line comments, which threads carry.
pub(super) fn review_comment(raw: &Value) -> Option<PullRequestComment> {
    let state = review_state(raw)?;
    let body = text(raw, "body").unwrap_or_default();
    if body.trim().is_empty()
        && matches!(
            state,
            PullRequestReviewState::Commented | PullRequestReviewState::Pending
        )
    {
        return None;
    }
    Some(PullRequestComment {
        id: format!("{REVIEW}{}", raw["id"].as_u64()?),
        author: actor(&raw["user"]),
        body,
        created_at: text(raw, "submitted_at")?,
        edited_at: None,
        url: text(raw, "html_url").filter(|url| !url.is_empty()),
        review_state: Some(state),
        reactions: Vec::new(),
        viewer_can_update: false,
        viewer_can_react: false,
    })
}

/// The side and line a review comment sits on: `position` on the new side, else
/// `original_position` on the old one.
fn placement(raw: &Value) -> Option<(ReviewSide, u32)> {
    let line = |field: &str| {
        raw[field]
            .as_u64()
            .filter(|line| *line > 0)
            .and_then(|line| u32::try_from(line).ok())
    };
    line("position")
        .map(|line| (ReviewSide::New, line))
        .or_else(|| line("original_position").map(|line| (ReviewSide::Old, line)))
}

/// Line comments gathered into one thread per file, side and line, as the servers' own pages
/// show them; a thread is named by its first comment, which replies and resolution address.
pub(super) fn threads(
    comments: &[Value],
    comment: impl Fn(&Value) -> Option<PullRequestComment>,
    viewer_can_reply: bool,
    viewer_can_resolve: bool,
) -> Vec<PullRequestReviewThread> {
    type Place = (String, Option<(ReviewSide, u32)>);
    let mut groups: Vec<(Place, Vec<&Value>)> = Vec::new();
    for raw in comments {
        let Some(path) = text(raw, "path") else {
            continue;
        };
        let at = (path, placement(raw));
        match groups.iter_mut().find(|(known, _)| *known == at) {
            Some((_, group)) => group.push(raw),
            None => groups.push((at, vec![raw])),
        }
    }
    groups
        .into_iter()
        .filter_map(|((path, place), mut group)| {
            group.sort_by_key(|raw| raw["id"].as_u64().unwrap_or(0));
            let first = group.first()?;
            let anchor = place.and_then(|(side, line)| {
                let revision = match side {
                    ReviewSide::New => "commit_id",
                    ReviewSide::Old => "original_commit_id",
                };
                Some(PullRequestReviewAnchor {
                    revision: text(first, revision).filter(|sha| !sha.is_empty())?,
                    path: path.clone(),
                    side,
                    start_line: line,
                    end_line: line,
                })
            });
            let comments: Vec<_> = group.iter().filter_map(|raw| comment(raw)).collect();
            Some(PullRequestReviewThread {
                id: first["id"].as_u64()?.to_string(),
                path,
                resolved: !first["resolver"].is_null(),
                outdated: false,
                anchor,
                diff_hunk: text(first, "diff_hunk").map(|hunk| {
                    let lines: Vec<_> = hunk
                        .lines()
                        .filter(|line| !line.starts_with("@@"))
                        .collect();
                    lines[lines.len().saturating_sub(4)..].join("\n")
                }),
                total_comments: comments.len() as u64,
                comments,
                replies_after: None,
                viewer_can_reply,
                viewer_can_resolve,
            })
        })
        .collect()
}

pub(super) fn labels(pr: &Value) -> Vec<PullRequestLabel> {
    pr["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|label| {
            Some(PullRequestLabel {
                id: label["id"].as_u64()?.to_string(),
                name: text(label, "name")?,
                color: text(label, "color").map(|color| color.trim_start_matches('#').to_owned()),
                description: text(label, "description").filter(|text| !text.is_empty()),
            })
        })
        .collect()
}

pub(super) fn user_reviewer(login: String) -> PullRequestReviewer {
    PullRequestReviewer {
        id: login.clone(),
        login,
        kind: PullRequestReviewerKind::User,
    }
}

/// Requests first, then each reviewer's latest verdict when not asked again.
pub(super) fn reviewer_states(pr: &Value, reviews: &[Value]) -> Vec<PullRequestReviewerState> {
    let mut states: Vec<PullRequestReviewerState> = pr["requested_reviewers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|user| {
            Some(PullRequestReviewerState {
                reviewer: user_reviewer(text(user, "login")?),
                avatar_url: text(user, "avatar_url"),
                verdict: None,
            })
        })
        .collect();
    let mut latest: BTreeMap<String, (&Value, PullRequestReviewState)> = BTreeMap::new();
    for review in reviews {
        let (Some(login), Some(state)) = (text(&review["user"], "login"), review_state(review))
        else {
            continue;
        };
        if matches!(
            state,
            PullRequestReviewState::Pending | PullRequestReviewState::Dismissed
        ) {
            continue;
        }
        latest.insert(login, (review, state));
    }
    for (login, (review, state)) in latest {
        let reviewer = user_reviewer(login);
        if states.iter().any(|known| known.reviewer == reviewer) {
            continue;
        }
        states.push(PullRequestReviewerState {
            reviewer,
            avatar_url: text(&review["user"], "avatar_url"),
            verdict: Some(state),
        });
    }
    states
}

pub(super) fn files(diff: &str) -> Option<Vec<PullRequestFile>> {
    crate::github::pull_request_reads::diff_files(diff)
}

pub(super) fn remark(raw: &Value, path: Option<&str>) -> Option<PullRequestRemark> {
    Some(PullRequestRemark {
        id: raw["id"].as_u64()?.to_string(),
        author: text(&raw["user"], "login"),
        body: text(raw, "body").unwrap_or_default(),
        created_at: text(raw, "created_at")?,
        edited_at: edited_at(raw),
        url: text(raw, "html_url").filter(|url| !url.is_empty()),
        path: path.map(str::to_owned),
        review_state: None,
    })
}

/// A review is news when it says something or gives a verdict; a bodiless comment review only
/// wraps line comments, read on their own.
pub(super) fn review_remark(raw: &Value) -> Option<PullRequestRemark> {
    let state = review_state(raw)?;
    let body = text(raw, "body").unwrap_or_default();
    let verdict = matches!(
        state,
        PullRequestReviewState::Approved
            | PullRequestReviewState::ChangesRequested
            | PullRequestReviewState::Dismissed
    );
    if state == PullRequestReviewState::Pending || (body.trim().is_empty() && !verdict) {
        return None;
    }
    Some(PullRequestRemark {
        id: format!("{REVIEW}{}", raw["id"].as_u64()?),
        author: text(&raw["user"], "login"),
        body,
        created_at: text(raw, "submitted_at")?,
        edited_at: None,
        url: text(raw, "html_url").filter(|url| !url.is_empty()),
        path: None,
        review_state: text(raw, "state"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A `warning` status fails the head's checks (#643: Forgejo and Gitea finish a status as
    /// `warning`, which Gitea's own combined state counts as a failure), and `mergeable: false`
    /// is unknown, never a conflict: the servers also answer it for a conflict check still
    /// running or failed and for a draft (#729, where it woke a watch for a false conflict).
    ///
    /// Shapes: `CombinedStatus` and `CommitStatus` in modules/structs/status.go and the states
    /// in modules/structs/commit_status.go (Forgejo v17.0; Gitea v28 has the same fields);
    /// `PullRequest.mergeable` in modules/structs/pull.go and `PullRequest.Mergeable` in
    /// models/issues/pull.go (Forgejo v17.0). Content is invented.
    #[test]
    fn a_warning_status_fails_and_mergeable_false_is_not_a_conflict() {
        let status = |id: u64, context: &str, state: &str, target_url: &str| {
            json!({
                "id": id,
                "status": state,
                "target_url": target_url,
                "description": "",
                "url": format!("https://forge.example.test/api/v1/repos/team/app/statuses/{id}"),
                "context": context,
                "creator": null,
                "created_at": "2026-10-09T08:00:00Z",
                "updated_at": "2026-10-09T08:05:00Z",
            })
        };
        let combined = json!({
            "state": "warning",
            "sha": "4f2c9e1b7a",
            "total_count": 3,
            "statuses": [
                status(3, "ci/build", "success", "https://ci.example.test/runs/3"),
                status(2, "ci/lint", "warning", "https://ci.example.test/runs/2"),
                status(1, "docs/preview", "pending", ""),
            ],
            "repository": null,
            "commit_url": "https://forge.example.test/api/v1/repos/team/app/commits/4f2c9e1b7a",
            "url": "https://forge.example.test/api/v1/repos/team/app/commits/4f2c9e1b7a/status",
        });
        let checks = checks(&combined);
        assert_eq!(
            checks
                .iter()
                .map(|check| (check.name.as_str(), check.status, check.url.as_deref()))
                .collect::<Vec<_>>(),
            [
                (
                    "ci/build",
                    CheckStatus::Success,
                    Some("https://ci.example.test/runs/3")
                ),
                (
                    "ci/lint",
                    CheckStatus::Failure,
                    Some("https://ci.example.test/runs/2")
                ),
                ("docs/preview", CheckStatus::Pending, None),
            ]
        );
        assert_eq!(checks_state(&checks), Some(ChecksState::Failing));

        let pull = |mergeable: bool, draft: bool| {
            json!({"number": 7, "state": "open", "draft": draft, "merged": false,
                   "mergeable": mergeable})
        };
        for draft in [false, true] {
            assert_eq!(mergeability(&pull(false, draft)), Mergeability::Unknown);
        }
        assert_eq!(mergeability(&pull(true, false)), Mergeability::Clean);
    }
}
