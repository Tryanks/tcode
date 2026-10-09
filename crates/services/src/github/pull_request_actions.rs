//! Writes to a linked pull request. Each request is sent once and answered in the domain's terms;
//! whatever a write may have changed is dropped from the reads, so the next read sees it.

use super::{
    CredentialError, GitHubError, RequestOptions, RestRequest,
    graphql::Document,
    pull_request_reads::{
        MAX_PAGES, PullRequestReads, Reader, is_revision, percent_encode, reaction_name,
    },
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::UNIX_EPOCH};
use tcode_core::{
    pull_request::{PullRequestKey, PullRequestReviewDraftComment},
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestFile, PullRequestFileText,
    PullRequestPatch, PullRequestRejection as Rejection, PullRequestReviewVerdict,
    PullRequestReviewerKind,
};

/// A pending comment's id and the revision its lines now read at, if they still do.
pub type Moved = (u64, Option<String>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchoring {
    InDiff,
    OutsideDiff,
    /// The pull request is at another head now.
    Moved,
}

/// Whether the lines fall inside one hunk of the file on that side; GitHub refuses a review
/// comment anywhere else.
fn in_hunks(
    files: &[PullRequestFile],
    path: &str,
    side: ReviewSide,
    (start, end): (u32, u32),
) -> bool {
    let Some(PullRequestPatch::Hunks(hunks)) = files
        .iter()
        .find(|file| file.path == path)
        .map(|file| &file.patch)
    else {
        return false;
    };
    hunks
        .lines()
        .filter_map(|line| line.strip_prefix("@@ "))
        .any(|header| {
            let mut ranges = header.split_whitespace();
            let range = match side {
                ReviewSide::Old => ranges.next().and_then(|range| range.strip_prefix('-')),
                ReviewSide::New => ranges.nth(1).and_then(|range| range.strip_prefix('+')),
            };
            let Some((first, count)) =
                range.map(|range| range.split_once(',').unwrap_or((range, "1")))
            else {
                return false;
            };
            let (Ok(first), Ok(count)) = (first.parse::<u32>(), count.parse::<u32>()) else {
                return false;
            };
            count > 0 && first <= start && end < first + count
        })
}

/// The reads a write may change.
#[derive(Clone, Copy)]
enum Affects {
    Conversation,
    Labels,
    Reviewers,
}

/// Why a write was not sent, or why GitHub refused it.
fn rejection(error: GitHubError) -> Rejection {
    match error {
        GitHubError::Credential(CredentialError::Disabled) => Rejection::HostDisabled,
        GitHubError::Credential(_) | GitHubError::Unauthorized => Rejection::NoCredential,
        GitHubError::RateLimited { retry_at, .. } | GitHubError::Paused { retry_at } => {
            Rejection::RateLimited {
                retry_at: retry_at
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            }
        }
        GitHubError::NotFound => Rejection::NotFound,
        GitHubError::Response { messages, .. } => Rejection::Refused { messages },
        GitHubError::InvalidInput => Rejection::Invalid,
        _ => Rejection::Failed,
    }
}

/// The answer to a write that was sent. Without an answer, or with a server failure, GitHub may
/// have applied it; a refusal or a rate limit says it did not.
fn answered<T>(result: Result<T, GitHubError>) -> Outcome {
    match result {
        Ok(_) => Outcome::Applied,
        Err(
            GitHubError::Request
            | GitHubError::Deadline
            | GitHubError::BodyTooLarge
            | GitHubError::InvalidResponse,
        ) => Outcome::Uncertain,
        Err(GitHubError::Response { status, .. }) if status >= 500 => Outcome::Uncertain,
        Err(error) => Outcome::Rejected(rejection(error)),
    }
}

fn blank(text: &str) -> bool {
    text.trim().is_empty()
}

fn side(side: ReviewSide) -> &'static str {
    match side {
        ReviewSide::Old => "LEFT",
        ReviewSide::New => "RIGHT",
    }
}

impl Reader<'_> {
    fn mutate(&self, operation: &'static str, query: &str, variables: Value) -> Outcome {
        let Value::Object(variables) = variables else {
            unreachable!("mutation variables are an object")
        };
        answered(self.api.graphql(
            &self.key.host,
            &Document {
                query: query.to_owned(),
                variables: variables.into_iter().collect::<BTreeMap<_, _>>(),
            },
            &RequestOptions {
                operation,
                ..self.options.clone()
            },
        ))
    }

    fn send(
        &self,
        operation: &'static str,
        method: &str,
        rest: &str,
        body: Option<&Value>,
    ) -> Outcome {
        answered(self.api.rest(
            &self.key.host,
            RestRequest {
                method,
                path: &self.rest_path(rest),
                body,
                if_none_match: None,
                accept: None,
            },
            &RequestOptions {
                operation,
                ..self.options.clone()
            },
        ))
    }

    /// The kind of node a client-given id names, once it is known to hang off this pull request:
    /// a mutation writes wherever the id belongs, whichever pull request the request names.
    fn subject(&self, id: &str) -> Result<String, Rejection> {
        let response = self
            .query(
                "PullRequestSubject",
                "query PullRequestSubject($owner: String!, $name: String!, $number: Int!, $subject: ID!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { id } } node(id: $subject) { __typename id ... on IssueComment { pullRequest { id } } ... on PullRequestReviewComment { pullRequest { id } } ... on PullRequestReview { pullRequest { id } } ... on PullRequestReviewThread { pullRequest { id } } } }".to_owned(),
                [("subject", json!(id))],
            )
            .map_err(rejection)?;
        let data = &response["data"];
        let expected = data["repository"]["pullRequest"]["id"].as_str();
        let node = &data["node"];
        let actual = node["pullRequest"]["id"].as_str().or(node["id"].as_str());
        if expected.is_none() || actual != expected {
            return Err(Rejection::ForeignSubject);
        }
        Ok(node["__typename"].as_str().unwrap_or_default().to_owned())
    }

    fn thread(&self, id: &str) -> Result<(), Rejection> {
        match self.subject(id)?.as_str() {
            "PullRequestReviewThread" => Ok(()),
            _ => Err(Rejection::Invalid),
        }
    }

    /// Labels come off one request each, since the endpoint names one in its path; the first
    /// failure stops the rest.
    fn remove_labels(&self, labels: &[String]) -> Outcome {
        let issue = format!("issues/{}/labels", self.key.number);
        for (index, label) in labels.iter().enumerate() {
            let outcome = self.send(
                "RemovePullRequestLabel",
                "DELETE",
                &format!("{issue}/{}", percent_encode(label)),
                None,
            );
            if outcome == Outcome::Applied {
                continue;
            }
            if index == 0 {
                return outcome;
            }
            return Outcome::Partial {
                applied: labels[..index].to_vec(),
                unapplied: labels[index..].to_vec(),
                failure: Box::new(outcome),
            };
        }
        Outcome::Applied
    }
}

impl PullRequestReads {
    /// Every action but a review, whose content is the runtime's draft: see [`Self::submit_review`].
    pub fn act(&self, key: &PullRequestKey, action: &PullRequestAction) -> Outcome {
        let affects = match action {
            PullRequestAction::AddLabels { .. } | PullRequestAction::RemoveLabels { .. } => {
                Affects::Labels
            }
            PullRequestAction::RequestReviewers { .. } => Affects::Reviewers,
            _ => Affects::Conversation,
        };
        let outcome = self.write(key, action).unwrap_or_else(Outcome::Rejected);
        self.written(key, affects, &outcome);
        outcome
    }

    fn write(
        &self,
        key: &PullRequestKey,
        action: &PullRequestAction,
    ) -> Result<Outcome, Rejection> {
        let reader = self.reader(key).map_err(rejection)?;
        Ok(match action {
            PullRequestAction::Comment { body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                let subject = self.node_id(&reader).map_err(rejection)?;
                reader.mutate(
                    "AddPullRequestComment",
                    "mutation AddPullRequestComment($subjectId: ID!, $body: String!) { addComment(input: { subjectId: $subjectId, body: $body }) { clientMutationId } }",
                    json!({"subjectId": subject, "body": body}),
                )
            }
            PullRequestAction::SubmitReview { .. } => return Err(Rejection::Invalid),
            PullRequestAction::ReplyToThread { thread_id, body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                reader.thread(thread_id)?;
                reader.mutate(
                    "ReplyToPullRequestThread",
                    "mutation ReplyToPullRequestThread($threadId: ID!, $body: String!) { addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $threadId, body: $body }) { comment { id } } }",
                    json!({"threadId": thread_id, "body": body}),
                )
            }
            PullRequestAction::ResolveThread {
                thread_id,
                resolved,
            } => {
                reader.thread(thread_id)?;
                let (operation, query) = if *resolved {
                    (
                        "ResolvePullRequestThread",
                        "mutation ResolvePullRequestThread($threadId: ID!) { resolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } } }",
                    )
                } else {
                    (
                        "UnresolvePullRequestThread",
                        "mutation UnresolvePullRequestThread($threadId: ID!) { unresolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } } }",
                    )
                };
                reader.mutate(operation, query, json!({"threadId": thread_id}))
            }
            PullRequestAction::React {
                subject_id,
                content,
                reacted,
            } => {
                if !matches!(
                    reader.subject(subject_id)?.as_str(),
                    "PullRequest"
                        | "IssueComment"
                        | "PullRequestReviewComment"
                        | "PullRequestReview"
                ) {
                    return Err(Rejection::Invalid);
                }
                let (operation, query) = if *reacted {
                    (
                        "AddPullRequestReaction",
                        "mutation AddPullRequestReaction($subjectId: ID!, $content: ReactionContent!) { addReaction(input: { subjectId: $subjectId, content: $content }) { reaction { content } } }",
                    )
                } else {
                    (
                        "RemovePullRequestReaction",
                        "mutation RemovePullRequestReaction($subjectId: ID!, $content: ReactionContent!) { removeReaction(input: { subjectId: $subjectId, content: $content }) { reaction { content } } }",
                    )
                };
                reader.mutate(
                    operation,
                    query,
                    json!({"subjectId": subject_id, "content": reaction_name(*content)}),
                )
            }
            PullRequestAction::EditComment { comment_id, body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                let (operation, query) = match reader.subject(comment_id)?.as_str() {
                    "IssueComment" => (
                        "EditPullRequestComment",
                        "mutation EditPullRequestComment($commentId: ID!, $body: String!) { updateIssueComment(input: { id: $commentId, body: $body }) { issueComment { id } } }",
                    ),
                    "PullRequestReviewComment" => (
                        "EditPullRequestReviewComment",
                        "mutation EditPullRequestReviewComment($commentId: ID!, $body: String!) { updatePullRequestReviewComment(input: { pullRequestReviewCommentId: $commentId, body: $body }) { pullRequestReviewComment { id } } }",
                    ),
                    _ => return Err(Rejection::Invalid),
                };
                reader.mutate(
                    operation,
                    query,
                    json!({"commentId": comment_id, "body": body}),
                )
            }
            PullRequestAction::Edit { title, body } => {
                if (title.is_none() && body.is_none()) || title.as_deref().is_some_and(blank) {
                    return Err(Rejection::Invalid);
                }
                let mut variables =
                    json!({"pullRequestId": self.node_id(&reader).map_err(rejection)?});
                // A variable left out puts no field in the input, which keeps GitHub's text;
                // an empty one would clear it.
                if let Some(title) = title {
                    variables["title"] = json!(title);
                }
                if let Some(body) = body {
                    variables["body"] = json!(body);
                }
                reader.mutate(
                    "EditPullRequest",
                    "mutation EditPullRequest($pullRequestId: ID!, $title: String, $body: String) { updatePullRequest(input: { pullRequestId: $pullRequestId, title: $title, body: $body }) { pullRequest { id } } }",
                    variables,
                )
            }
            PullRequestAction::AddLabels { labels } => {
                if labels.is_empty() {
                    return Err(Rejection::Invalid);
                }
                // A pull request is an issue to the labels API, which adds to what is there.
                reader.send(
                    "AddPullRequestLabels",
                    "POST",
                    &format!("issues/{}/labels", key.number),
                    Some(&json!({"labels": labels})),
                )
            }
            PullRequestAction::RemoveLabels { labels } => {
                if labels.is_empty() {
                    return Err(Rejection::Invalid);
                }
                reader.remove_labels(labels)
            }
            PullRequestAction::RequestReviewers {
                reviewers,
                requested,
            } => {
                if reviewers.is_empty() {
                    return Err(Rejection::Invalid);
                }
                let named = |kind| {
                    reviewers
                        .iter()
                        .filter(|reviewer| reviewer.kind == kind)
                        .map(|reviewer| reviewer.login.as_str())
                        .collect::<Vec<_>>()
                };
                // GitHub takes a request back from exactly whoever it was made of, so both
                // methods send the same body.
                reader.send(
                    "RequestPullRequestReviewers",
                    if *requested { "POST" } else { "DELETE" },
                    &format!("pulls/{}/requested_reviewers", key.number),
                    Some(&json!({
                        "reviewers": named(PullRequestReviewerKind::User),
                        "team_reviewers": named(PullRequestReviewerKind::Team),
                    })),
                )
            }
        })
    }

    /// The whole review in one request, so nothing of it is visible until the verdict is sent.
    /// The head is read fresh first: a review anchored at `head` is not sent once the pull
    /// request has moved past it, and an unplaced comment is never sent.
    pub fn submit_review(
        &self,
        key: &PullRequestKey,
        verdict: PullRequestReviewVerdict,
        head: &str,
        body: &str,
        comments: &[PullRequestReviewDraftComment],
    ) -> Outcome {
        let outcome = self
            .review(key, verdict, head, body, comments)
            .unwrap_or_else(Outcome::Rejected);
        self.written(key, Affects::Conversation, &outcome);
        outcome
    }

    fn review(
        &self,
        key: &PullRequestKey,
        verdict: PullRequestReviewVerdict,
        head: &str,
        body: &str,
        comments: &[PullRequestReviewDraftComment],
    ) -> Result<Outcome, Rejection> {
        if !is_revision(head)
            || comments.iter().any(|comment| !comment.placed)
            || (verdict != PullRequestReviewVerdict::Approve && blank(body) && comments.is_empty())
        {
            return Err(Rejection::Invalid);
        }
        let reader = self.reader(key).map_err(rejection)?;
        self.revisions.invalidate(key);
        let current = self.revisions(&reader).map_err(rejection)?.head.clone();
        if current != head {
            return Err(Rejection::StaleHead { head: current });
        }
        let event = match verdict {
            PullRequestReviewVerdict::Comment => "COMMENT",
            PullRequestReviewVerdict::Approve => "APPROVE",
            PullRequestReviewVerdict::RequestChanges => "REQUEST_CHANGES",
        };
        let comments: Vec<_> = comments
            .iter()
            .map(|comment| {
                let mut line = json!({
                    "path": comment.path,
                    "line": comment.end_line,
                    "side": side(comment.side),
                    "body": comment.body,
                });
                if comment.start_line < comment.end_line {
                    line["start_line"] = json!(comment.start_line);
                    line["start_side"] = json!(side(comment.side));
                }
                line
            })
            .collect();
        Ok(reader.send(
            "SubmitPullRequestReview",
            "POST",
            &format!("pulls/{}/reviews", key.number),
            Some(&json!({
                "commit_id": current,
                "event": event,
                "body": body,
                "comments": comments,
            })),
        ))
    }

    /// The changed files at the pull request's current revisions, every page of them.
    fn diff(
        &self,
        key: &PullRequestKey,
    ) -> Result<(String, String, Vec<PullRequestFile>), GitHubError> {
        let first = self.files(key, None)?.value;
        let (base, head) = (first.base.clone(), first.head.clone());
        let mut files = first.files.clone();
        let mut next = first.next_page;
        while let Some(page) = next.filter(|page| (*page as usize) <= MAX_PAGES) {
            let more = self.files(key, Some(page))?.value;
            files.extend(more.files.iter().cloned());
            next = more.next_page;
        }
        Ok((base, head, files))
    }

    /// Whether GitHub takes a review comment on these lines: the pull request is still at
    /// `head`, and they lie inside one of the file's hunks on that side.
    pub fn commentable(
        &self,
        key: &PullRequestKey,
        head: &str,
        path: &str,
        side: ReviewSide,
        lines: (u32, u32),
    ) -> Result<Anchoring, GitHubError> {
        let (_, current, files) = self.diff(key)?;
        if current != head {
            return Ok(Anchoring::Moved);
        }
        Ok(if in_hunks(&files, path, side, lines) {
            Anchoring::InDiff
        } else {
            Anchoring::OutsideDiff
        })
    }

    /// The pull request's current head, and where each comment's lines are at it: the revision
    /// its side now shows when the lines read the same and are still in the diff, else `None`.
    pub fn reanchor(
        &self,
        key: &PullRequestKey,
        comments: &[PullRequestReviewDraftComment],
    ) -> Result<(String, Vec<Moved>), GitHubError> {
        let (base, head, files) = self.diff(key)?;
        let lines = |revision: &str, comment: &PullRequestReviewDraftComment| {
            let text = self.file_text(key, revision, &comment.path)?.value;
            Ok::<_, GitHubError>(match &*text {
                PullRequestFileText::Text(text) => {
                    let lines: Vec<_> = text.lines().collect();
                    lines
                        .get(comment.start_line as usize - 1..comment.end_line as usize)
                        .map(|lines| lines.join("\n"))
                }
                _ => None,
            })
        };
        let mut moved = Vec::new();
        for comment in comments {
            let revision = match comment.side {
                ReviewSide::Old => &base,
                ReviewSide::New => &head,
            };
            let kept = comment.placed
                && in_hunks(
                    &files,
                    &comment.path,
                    comment.side,
                    (comment.start_line, comment.end_line),
                )
                && (comment.revision == *revision || {
                    let before = lines(&comment.revision, comment)?;
                    before.is_some() && before == lines(revision, comment)?
                });
            moved.push((comment.id, kept.then(|| revision.clone())));
        }
        Ok((head, moved))
    }

    fn written(&self, key: &PullRequestKey, affects: Affects, outcome: &Outcome) {
        if matches!(outcome, Outcome::Rejected(_)) {
            return;
        }
        match affects {
            Affects::Conversation => {
                self.conversations.invalidate(key);
                self.replies.invalidate(key);
            }
            Affects::Labels => self.labels.invalidate(key),
            Affects::Reviewers => self.reviewers.invalidate(key),
        }
    }
}
