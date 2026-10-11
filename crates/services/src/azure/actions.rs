//! Writes to an Azure DevOps pull request. Each is sent once; an answer that cannot say
//! whether Azure applied it is uncertain, never retried.

use super::{
    api::{PREVIEW, Request, Response},
    reads::{PULL_REQUEST, Pr, head, merge_strategy, text, votes},
};
use crate::forge::{ForgeError, Step, answered, in_order};
use serde_json::{Value, json};
use tcode_core::{
    pull_request::{PullRequestMergeMethod, PullRequestReviewDraftComment},
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestFile, PullRequestPatch,
    PullRequestRejection as Rejection, PullRequestReviewVerdict,
};

/// The identity Azure takes as nobody, which cancels auto-complete.
const NOBODY: &str = "00000000-0000-0000-0000-000000000000";

/// The votes a review casts: approve, and wait for the author, which is what asking for
/// changes is on Azure; rejecting (-10) is a stronger refusal no Tcode verdict means.
fn vote(verdict: PullRequestReviewVerdict) -> Option<i64> {
    match verdict {
        PullRequestReviewVerdict::Approve => Some(10),
        PullRequestReviewVerdict::RequestChanges => Some(-5),
        PullRequestReviewVerdict::Comment => None,
    }
}

impl Pr<'_> {
    fn write(
        &self,
        method: &str,
        path: String,
        body: Option<Value>,
        operation: &'static str,
    ) -> Result<Response, ForgeError> {
        self.api.send(
            self.authority(),
            Request::write(method, path, body, operation),
        )
    }

    fn update(&self, body: Value, operation: &'static str) -> Result<Response, ForgeError> {
        self.write("PATCH", self.path(""), Some(body), operation)
    }

    /// The pull request at `head`, or the head it moved to.
    fn at_head(&self, at: &str) -> Result<Value, Rejection> {
        let pr = self.pr().map_err(ForgeError::rejection)?;
        let current = head(&pr).ok_or(Rejection::Failed)?;
        if current != at {
            return Err(Rejection::StaleHead { head: current });
        }
        Ok(pr)
    }

    /// The reading account's identity id, which votes and auto-complete name.
    fn viewer_id(&self) -> Result<String, Rejection> {
        self.viewer()
            .map_err(ForgeError::rejection)?
            .and_then(|viewer| text(&viewer, "id"))
            .ok_or(Rejection::NoCredential)
    }

    fn new_thread(&self, body: &str, context: Option<Value>, operation: &'static str) -> Outcome {
        let mut thread = json!({
            "comments": [{ "parentCommentId": 0, "content": body, "commentType": 1 }],
            "status": 1,
        });
        if let Some(context) = context {
            thread["threadContext"] = context;
        }
        answered(self.write("POST", self.path("/threads"), Some(thread), operation))
    }

    fn merge(
        &self,
        at: &str,
        method: PullRequestMergeMethod,
        auto: bool,
        remove_credits: bool,
    ) -> Result<Outcome, Rejection> {
        let pr = self.at_head(at)?;
        let mut options = json!({
            "mergeStrategy": merge_strategy(method),
            "deleteSourceBranch": pr["completionOptions"]["deleteSourceBranch"].as_bool().unwrap_or(false),
        });
        // Rebasing keeps each commit's own message. Azure's page offers the title and the
        // description as the message; one with agents' credits in it is sent without them.
        if remove_credits && method != PullRequestMergeMethod::Rebase {
            let mut message = format!(
                "Merged PR {}: {}",
                self.key.number,
                text(&pr, "title").unwrap_or_default()
            );
            if let Some(description) =
                text(&pr, "description").filter(|text| !text.trim().is_empty())
            {
                message.push_str("\n\n");
                message.push_str(&description);
            }
            let cleaned = crate::github::remove_agent_credits(&message);
            if cleaned != message {
                options["mergeCommitMessage"] = json!(cleaned);
            }
        }
        if auto {
            let viewer = self.viewer_id()?;
            return Ok(
                match self.update(
                    json!({ "autoCompleteSetBy": { "id": viewer }, "completionOptions": options }),
                    "AutoComplete",
                ) {
                    Ok(_) => Outcome::AutoMergeEnabled { method },
                    Err(error) => answered::<()>(Err(error)),
                },
            );
        }
        // Azure completes only at the head it is told, so a push in between is refused.
        let body = json!({
            "status": "completed",
            "lastMergeSourceCommit": { "commitId": at },
            "completionOptions": options,
        });
        Ok(match self.update(body, "Complete") {
            Ok(response) => match response.json::<Value>() {
                Ok(answer) if answer["status"].as_str() == Some("completed") => Outcome::Applied,
                // Taken, and merged once Azure's merge of it finishes.
                _ => Outcome::Uncertain,
            },
            Err(error) => answered::<()>(Err(error)),
        })
    }

    /// Adds the reviewers not on the pull request yet, as optional, and removes the others, one
    /// request each; a reviewer already there keeps their vote.
    fn set_reviewers(
        &self,
        add: &[tcode_protocol::PullRequestReviewer],
        remove: &[tcode_protocol::PullRequestReviewer],
    ) -> Result<Outcome, Rejection> {
        let held = votes(&self.pr().map_err(ForgeError::rejection)?);
        let mut steps: Vec<Step> = Vec::new();
        for reviewer in add
            .iter()
            .filter(|reviewer| !held.contains_key(&reviewer.id))
        {
            let path = self.path(&format!("/reviewers/{}", identity(&reviewer.id)?));
            let body = json!({ "id": reviewer.id, "vote": 0, "isRequired": false });
            steps.push((
                vec![reviewer.login.clone()],
                Box::new(move || answered(self.write("PUT", path, Some(body), "AddReviewer"))),
            ));
        }
        for reviewer in remove {
            let path = self.path(&format!("/reviewers/{}", identity(&reviewer.id)?));
            steps.push((
                vec![reviewer.login.clone()],
                Box::new(move || answered(self.write("DELETE", path, None, "RemoveReviewer"))),
            ));
        }
        Ok(in_order(steps))
    }

    fn set_labels(&self, add: &[String], remove: &[String]) -> Result<Outcome, Rejection> {
        let mut steps: Vec<Step> = Vec::new();
        for name in add {
            if blank(name) {
                return Err(Rejection::Invalid);
            }
            let path = self.path("/labels");
            let body = json!({ "name": name });
            steps.push((
                vec![name.clone()],
                Box::new(move || {
                    answered(self.api.send(
                        self.authority(),
                        Request {
                            version: PREVIEW,
                            ..Request::write("POST", path, Some(body), "AddLabel")
                        },
                    ))
                }),
            ));
        }
        for name in remove {
            let path = self.path(&format!(
                "/labels/{}",
                crate::github::pull_request_reads::percent_encode(name)
            ));
            steps.push((
                vec![name.clone()],
                Box::new(move || {
                    answered(self.api.send(
                        self.authority(),
                        Request {
                            version: PREVIEW,
                            ..Request::write("DELETE", path, None, "RemoveLabel")
                        },
                    ))
                }),
            ));
        }
        Ok(in_order(steps))
    }
}

/// An identity id as a path segment: Azure's are GUIDs.
fn identity(id: &str) -> Result<&str, Rejection> {
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        .then_some(id)
        .ok_or(Rejection::Invalid)
}

fn blank(text: &str) -> bool {
    text.trim().is_empty()
}

/// A comment id, `{thread}:{comment}`.
fn comment_id(id: &str) -> Result<(u64, u64), Rejection> {
    let (thread, comment) = id.split_once(':').ok_or(Rejection::Invalid)?;
    Ok((
        thread.parse().map_err(|_| Rejection::Invalid)?,
        comment.parse().map_err(|_| Rejection::Invalid)?,
    ))
}

pub(super) fn act(pr: &Pr<'_>, action: &PullRequestAction) -> Outcome {
    let result = || -> Result<Outcome, Rejection> {
        Ok(match action {
            PullRequestAction::Comment { body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                pr.new_thread(body, None, "Comment")
            }
            // The path names the pull request, so Azure answers not found for another's thread.
            PullRequestAction::ReplyToThread { thread_id, body } => {
                let thread: u64 = thread_id.parse().map_err(|_| Rejection::Invalid)?;
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                answered(pr.write(
                    "POST",
                    pr.path(&format!("/threads/{thread}/comments")),
                    Some(json!({ "parentCommentId": 1, "content": body, "commentType": 1 })),
                    "Reply",
                ))
            }
            PullRequestAction::ResolveThread {
                thread_id,
                resolved,
            } => {
                let thread: u64 = thread_id.parse().map_err(|_| Rejection::Invalid)?;
                answered(pr.write(
                    "PATCH",
                    pr.path(&format!("/threads/{thread}")),
                    Some(json!({ "status": if *resolved { "fixed" } else { "active" } })),
                    "Resolve",
                ))
            }
            PullRequestAction::EditComment {
                comment_id: id,
                body,
            } => {
                if blank(body) || id == PULL_REQUEST {
                    return Err(Rejection::Invalid);
                }
                let (thread, comment) = comment_id(id)?;
                answered(pr.write(
                    "PATCH",
                    pr.path(&format!("/threads/{thread}/comments/{comment}")),
                    Some(json!({ "content": body })),
                    "EditComment",
                ))
            }
            PullRequestAction::Edit { title, body } => {
                let mut fields = serde_json::Map::new();
                if let Some(title) = title {
                    if blank(title) {
                        return Err(Rejection::Invalid);
                    }
                    fields.insert("title".into(), json!(title));
                }
                if let Some(body) = body {
                    fields.insert("description".into(), json!(body));
                }
                if fields.is_empty() {
                    return Err(Rejection::Invalid);
                }
                answered(pr.update(Value::Object(fields), "Edit"))
            }
            PullRequestAction::SetLabels { add, remove } => {
                if add.is_empty() && remove.is_empty() {
                    return Err(Rejection::Invalid);
                }
                pr.set_labels(add, remove)?
            }
            PullRequestAction::SetReviewers { add, remove } => {
                if add.is_empty() && remove.is_empty() {
                    return Err(Rejection::Invalid);
                }
                pr.set_reviewers(add, remove)?
            }
            PullRequestAction::ReadyForReview | PullRequestAction::ConvertToDraft => {
                let draft = matches!(action, PullRequestAction::ConvertToDraft);
                answered(pr.update(json!({ "isDraft": draft }), "SetDraft"))
            }
            PullRequestAction::Close => {
                answered(pr.update(json!({ "status": "abandoned" }), "Abandon"))
            }
            PullRequestAction::Reopen => {
                answered(pr.update(json!({ "status": "active" }), "Reactivate"))
            }
            PullRequestAction::Merge {
                head,
                method,
                auto,
                remove_credits,
            } => pr.merge(head, *method, *auto, *remove_credits)?,
            PullRequestAction::DisableAutoMerge => answered(pr.update(
                json!({ "autoCompleteSetBy": { "id": NOBODY } }),
                "CancelAutoComplete",
            )),
            PullRequestAction::React { .. }
            | PullRequestAction::UpdateBranch { .. }
            | PullRequestAction::Revert => return Err(Rejection::Unsupported),
            PullRequestAction::SubmitReview { .. }
            | PullRequestAction::MergeStack { .. }
            | PullRequestAction::RebaseStack { .. } => return Err(Rejection::Invalid),
        })
    };
    result().unwrap_or_else(Outcome::Rejected)
}

/// The length of a line on one side of a file's hunks, `None` outside them.
fn line_length(
    files: &[PullRequestFile],
    path: &str,
    side: ReviewSide,
    line: u32,
) -> Option<usize> {
    let PullRequestPatch::Hunks(hunks) = &files.iter().find(|file| file.path == path)?.patch else {
        return None;
    };
    let (mut old, mut new) = (0u32, 0u32);
    for raw in hunks.lines() {
        if let Some(header) = raw.strip_prefix("@@ ") {
            let mut ranges = header.split_whitespace();
            let start = |range: Option<&str>, sign: char| {
                range?
                    .strip_prefix(sign)?
                    .split(',')
                    .next()?
                    .parse::<u32>()
                    .ok()
            };
            old = start(ranges.next(), '-')?;
            new = start(ranges.next(), '+')?;
            continue;
        }
        let (on_old, on_new) = match raw.chars().next() {
            Some('\\') => continue,
            Some('-') => (true, false),
            Some('+') => (false, true),
            _ => (true, true),
        };
        let at = match side {
            ReviewSide::Old if on_old => Some(old),
            ReviewSide::New if on_new => Some(new),
            _ => None,
        };
        if at == Some(line) {
            return Some(raw.chars().count().saturating_sub(1));
        }
        old += u32::from(on_old);
        new += u32::from(on_new);
    }
    None
}

/// Azure has no review: each line comment opens a thread on its lines of the diff the pull
/// request has now, then the summary opens one of its own, then the vote is cast, so a review
/// cut short is never an approval. One that stops part way is [`Outcome::Partial`].
pub(super) fn submit_review(
    pr: &Pr<'_>,
    files: &[PullRequestFile],
    verdict: PullRequestReviewVerdict,
    at: &str,
    body: &str,
    comments: &[PullRequestReviewDraftComment],
) -> Outcome {
    if verdict == PullRequestReviewVerdict::Comment && blank(body) && comments.is_empty() {
        return Outcome::Rejected(Rejection::Invalid);
    }
    if let Err(rejection) = pr.at_head(at) {
        return Outcome::Rejected(rejection);
    }
    let voter = match vote(verdict).map(|vote| pr.viewer_id().map(|id| (id, vote))) {
        Some(Err(rejection)) => return Outcome::Rejected(rejection),
        Some(Ok(voter)) => Some(voter),
        None => None,
    };
    let mut steps: Vec<Step> = Vec::new();
    for comment in comments {
        let lines = (comment.start_line, comment.end_line);
        if !crate::github::pull_request_actions::in_hunks(files, &comment.path, comment.side, lines)
        {
            return Outcome::Rejected(Rejection::Invalid);
        }
        let (start, end) = match comment.side {
            ReviewSide::New => ("rightFileStart", "rightFileEnd"),
            ReviewSide::Old => ("leftFileStart", "leftFileEnd"),
        };
        // Offsets are columns from 1; the range ends after the last line's last character.
        let end_offset =
            line_length(files, &comment.path, comment.side, comment.end_line).unwrap_or(0) + 1;
        let context = json!({
            "filePath": format!("/{}", comment.path),
            start: { "line": comment.start_line, "offset": 1 },
            end: { "line": comment.end_line, "offset": end_offset },
        });
        let text = comment.body.clone();
        steps.push((
            vec![format!("{}:{}", comment.path, comment.end_line)],
            Box::new(move || pr.new_thread(&text, Some(context), "ReviewComment")),
        ));
    }
    if !blank(body) {
        steps.push((
            vec!["summary".into()],
            Box::new(move || pr.new_thread(body, None, "ReviewSummary")),
        ));
    }
    if let Some((viewer, vote)) = voter {
        let name = match verdict {
            PullRequestReviewVerdict::Approve => "approval",
            _ => "request changes",
        };
        let path = pr.path(&format!("/reviewers/{viewer}"));
        steps.push((
            vec![name.into()],
            Box::new(move || {
                answered(pr.write("PUT", path, Some(json!({ "vote": vote })), "Vote"))
            }),
        ));
    }
    in_order(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A review approves with Azure's approval and asks for changes by waiting for the author;
    /// a comment-only review casts no vote.
    #[test]
    fn verdicts_cast_azures_votes() {
        assert_eq!(vote(PullRequestReviewVerdict::Approve), Some(10));
        assert_eq!(vote(PullRequestReviewVerdict::RequestChanges), Some(-5));
        assert_eq!(vote(PullRequestReviewVerdict::Comment), None);
    }
}
