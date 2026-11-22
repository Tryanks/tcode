//! A call that linked a pull request to the thread it was made in:
//! `link_pull_request` of Tcode's own `tcode_pull_requests` server, or
//! `tcode_control`'s `pull_request` linking on `self`.

use agent::{ItemStatus, McpRegistration};
use serde::Deserialize;
use tcode_core::pull_request::PullRequestKey;
use tcode_core::session::TimelineEntry;

use super::Call;

/// The pull request a call linked to its own thread.
pub(in crate::chat) struct Linked {
    pub(in crate::chat) key: PullRequestKey,
    /// The thread had it linked before the call.
    pub(in crate::chat) already: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Linking {
    host: String,
    repository: String,
    number: u64,
    #[serde(default)]
    already_linked: bool,
}

#[derive(Deserialize)]
struct ControlInput<'a> {
    #[serde(borrow)]
    thread_id: &'a str,
    #[serde(borrow)]
    op: &'a str,
    #[serde(default, borrow)]
    reference: Option<&'a str>,
    #[serde(default, borrow)]
    key: Option<&'a str>,
}

/// `tcode_control`'s answer to a link: what the host's command answered,
/// for the thread it named.
#[derive(Deserialize)]
struct ControlAnswer {
    thread_id: String,
    outcome: String,
    output: Option<ControlLinked>,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
enum ControlLinked {
    PullRequestLinked {
        key: PullRequestKey,
        already_linked: bool,
    },
}

/// A `tcode_control` link the thread made to itself: the thread's id as the
/// host answered it, and the reference the call named.
struct SelfLink<'a> {
    thread_id: String,
    reference: &'a str,
    linked: Linked,
}

/// Only a call that names `self` is known here to link its own thread: one
/// that names a thread by its id may name another, and stays a tool row.
fn self_link<'a>(call: &Call<'a>) -> Option<SelfLink<'a>> {
    if !call.served(&[McpRegistration::SERVER_NAME_CONTROL], &["pull_request"]) {
        return None;
    }
    let input = call.input::<ControlInput<'a>>()?;
    if input.thread_id != "self" || input.op != "link" {
        return None;
    }
    let answer = serde_json::from_str::<ControlAnswer>(call.output?.trim()).ok()?;
    if answer.outcome != "applied" {
        return None;
    }
    let ControlLinked::PullRequestLinked {
        key,
        already_linked,
    } = answer.output?;
    Some(SelfLink {
        thread_id: answer.thread_id,
        reference: input.reference.or(input.key)?,
        linked: Linked {
            key,
            already: already_linked,
        },
    })
}

fn linking(call: &Call<'_>) -> Option<Linked> {
    if !call.served(
        &[McpRegistration::SERVER_NAME_PULL_REQUESTS],
        &["link_pull_request"],
    ) {
        return None;
    }
    let linking = serde_json::from_str::<Linking>(call.output?.trim()).ok()?;
    let key = PullRequestKey::deserialize(serde_json::json!({
        "host": linking.host,
        "repository": linking.repository,
        "number": linking.number,
    }))
    .ok()?;
    Some(Linked {
        key,
        already: linking.already_linked,
    })
}

/// The pull request `entry` linked to its thread, which it stands in the
/// thread as a record of. A call still running has linked nothing yet, and
/// one that failed stays a tool row that shows what it reported.
pub(in crate::chat) fn card(entry: &TimelineEntry) -> Option<Linked> {
    let call = Call::of(entry)?;
    if call.status != ItemStatus::Completed {
        return None;
    }
    linking(&call).or_else(|| self_link(&call).map(|link| link.linked))
}

/// Whether `action` is the host's record of a link one of `entries` shows as
/// its card: the thread linking a pull request to itself, which the card
/// already tells.
pub(in crate::chat) fn told_by_card(
    action: &agent::ControlAction,
    entries: &[std::sync::Arc<TimelineEntry>],
) -> bool {
    action.action == tcode_core::control::action::PR_LINK
        && entries.iter().any(|entry| {
            Call::of(entry)
                .filter(|call| call.status == ItemStatus::Completed)
                .and_then(|call| self_link(&call))
                .is_some_and(|link| {
                    link.thread_id == action.actor_thread_id
                        && action.detail.as_deref() == Some(link.reference)
                })
        })
}

#[cfg(test)]
mod tests {
    use crate::chat::model::{Segment, segment_entries};
    use agent::{ItemContent, ItemStatus};
    use serde_json::Value;
    use std::sync::Arc;
    use tcode_core::session::{EntryContent, TimelineEntry};

    fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
        Arc::new(TimelineEntry {
            id: id.into(),
            content,
            ts: None,
            finished_ts: None,
            background: false,
            turn: 0,
        })
    }

    fn call(id: &str, name: &str, input: Value, output: &str) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::Item(ItemContent::ToolCall {
                name: name.into(),
                input,
                output: Some(output.into()),
                status: ItemStatus::Completed,
                image_reads: Vec::new(),
                family: None,
            }),
        )
    }

    fn audit(id: &str, actor: &str, detail: &str) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::ControlAction(agent::ControlAction {
                actor_thread_id: actor.into(),
                actor_title: String::new(),
                action: tcode_core::control::action::PR_LINK.into(),
                detail: Some(detail.into()),
            }),
        )
    }

    /// A link the thread made to itself is its record, and the host's audit
    /// of it in the same thread goes; a link to another thread, and another
    /// thread's link to this one, stay a tool row and a divider. The answers
    /// are the runtime's: `link_pull_request`'s object, and `tcode_control`'s
    /// action response around the host's command answer.
    #[test]
    fn a_link_to_the_thread_itself_is_its_record_and_its_audit_goes() {
        let answer = |thread: &str| {
            format!(
                r#"{{"thread_id":"{thread}","outcome":"applied","output":{{"type":"pull_request_linked","content":{{"key":{{"host":"github.com","repository":"o/r","number":8}},"already_linked":false}}}}}}"#
            )
        };
        let control = |id: &str, thread: &str, answered: &str| {
            call(
                id,
                "mcp__tcode_control__pull_request",
                serde_json::json!({"thread_id": thread, "op": "link", "reference": "o/r#8"}),
                &answer(answered),
            )
        };
        let entries = [
            call(
                "linked",
                "mcp__tcode_pull_requests__link_pull_request",
                serde_json::json!({"url": "https://github.com/o/r/pull/7"}),
                r#"{"host":"github.com","repository":"o/r","number":7,"url":"https://github.com/o/r/pull/7","alreadyLinked":true,"wasLinked":false}"#,
            ),
            control("own", "self", "me"),
            audit("own-audit", "me", "o/r#8"),
            control("other", "them", "them"),
            audit("other-audit", "me", "Them — o/r#8"),
            audit("linked-here", "someone", "o/r#8"),
        ];
        let flow = segment_entries(&entries, false).flow;
        let shown = flow
            .iter()
            .map(|segment| match segment {
                Segment::PullRequest(entry) => format!("record {}", entry.id),
                Segment::ControlAction(entry) => format!("divider {}", entry.id),
                Segment::ActivityRun(run) => format!(
                    "run {}",
                    run.iter()
                        .map(|entry| entry.id.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shown,
            [
                "record linked",
                "record own",
                "run other",
                "divider other-audit",
                "divider linked-here",
            ]
        );
        let record = super::card(&entries[0]).expect("a link is its record");
        assert_eq!(
            (
                record.key.repository.as_str(),
                record.key.number,
                record.already
            ),
            ("o/r", 7, true)
        );
    }
}
