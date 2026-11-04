//! A subagent's window: the agent a call started and the task it was given,
//! and how it stands. The agent's work is its own thread's — the host routes
//! its calls there and the lead's timeline leaves them out — so a press on
//! the window opens that thread rather than unfolding anything here.

use agent::{ItemContent, ItemStatus};
use gpui::App;
use tcode_core::session::{EntryContent, TimelineEntry};

use super::Activity;
use super::window::{Argument, Chip, Header, Verb};
use crate::chat::ChatView;
use crate::icon::IconName;

/// The window a subagent item shows as: the agent's type in
/// the verb's weight, its task, the model and effort it ran with as one
/// chip, which gives way as one in a narrow row. A failed agent's report is
/// its error, told by its first line.
pub(in crate::chat) fn activity(
    agent_type: &str,
    description: &str,
    status: ItemStatus,
    summary: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Activity {
    let summary = summary.filter(|summary| !summary.trim().is_empty());
    let ran_with: Vec<&str> = [model, effort].into_iter().flatten().collect();
    let mut header = Header::new(IconName::Bot, Verb("agent"))
        .argument(Argument::plain(crate::chat::model::one_line(description)))
        .chip((!ran_with.is_empty()).then(|| Chip::Tag(ran_with.join(" · "))));
    if !agent_type.is_empty() {
        header = header.subject(agent_type.to_owned());
    }
    let mut copy = Vec::new();
    super::copy(&mut copy, "tools.copy_task", description);
    if let Some(summary) = summary {
        super::copy(&mut copy, "tools.copy_summary", summary);
    }
    Activity {
        failure: super::window::failure_line(&header, status, summary),
        header,
        status,
        copy,
        expandable: false,
        path: None,
    }
}

/// The thread the host keeps a subagent's own work in, once it has one.
/// `None` for any other entry.
pub(in crate::chat) fn child_thread(
    chat: &ChatView,
    entry: &TimelineEntry,
    cx: &App,
) -> Option<String> {
    if !matches!(
        &entry.content,
        EntryContent::Item(ItemContent::Subagent { .. })
    ) {
        return None;
    }
    let lead = chat.session_id.as_deref()?;
    chat.workspace_store
        .read(cx)
        .sidebar_sessions()
        .iter()
        .find(|meta| {
            meta.parent_session_id.as_deref() == Some(lead)
                && meta.native_subagent.as_deref() == Some(entry.id.as_str())
        })
        .map(|meta| meta.id.clone())
}
