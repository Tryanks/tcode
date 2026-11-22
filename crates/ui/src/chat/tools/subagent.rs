//! What a call started that works on its own: a subagent of the harness, or
//! a thread Tcode started for a `tcode_control` call. Each stands as a card
//! of its own in the flow rather than a window among the turn's work, and
//! the card opens where that work is: a subagent's calls go to the thread
//! the host keeps for it, which the lead's timeline leaves out.

use agent::{ItemContent, ItemStatus};
use gpui::App;
use tcode_core::session::{EntryContent, TimelineEntry};

use super::threads;
use crate::chat::timeline::TimelineView;

/// What `entry` started, as its card tells it.
pub(in crate::chat) enum Started<'a> {
    /// A subagent of the harness's own.
    Native {
        agent_type: &'a str,
        description: &'a str,
        status: ItemStatus,
        summary: Option<&'a str>,
        model: Option<&'a str>,
        effort: Option<&'a str>,
    },
    /// A thread Tcode starts for the call.
    Thread(threads::Start<'a>),
}

/// What `entry` started, when it stands as a card.
pub(in crate::chat) fn card(entry: &TimelineEntry) -> Option<Started<'_>> {
    match &entry.content {
        EntryContent::Item(ItemContent::Subagent {
            agent_type,
            description,
            status,
            summary,
            model,
            effort,
        }) => Some(Started::Native {
            agent_type,
            description,
            status: *status,
            summary: summary.as_deref().filter(|text| !text.trim().is_empty()),
            model: model.as_deref(),
            effort: effort.as_deref(),
        }),
        EntryContent::Item(ItemContent::ToolCall { .. }) => {
            threads::start(entry).map(Started::Thread)
        }
        _ => None,
    }
}

/// The thread the host keeps a subagent's own work in, once it has one.
/// `None` for any other entry.
pub(in crate::chat) fn child_thread(
    chat: &TimelineView,
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
