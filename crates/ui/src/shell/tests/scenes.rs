//! A pane switched to another thread draws it as a new scene: what the
//! thread before left moving, and what the thread shown now did when it was
//! last on screen, carry nothing into the frame that first draws it.

use super::*;

/// `id`'s status and its log, as the host answers the thread's subscriptions.
pub(super) fn serve_log(
    host: &MountedShell,
    status: tcode_protocol::SessionStatus,
    events: Vec<agent::AgentEvent>,
) {
    let records: Vec<tcode_core::session::StoredEvent> =
        events.into_iter().map(Into::into).collect();
    let session_id = status.session_id.clone();
    send_events(
        host,
        vec![
            (
                Topic::SessionStatus {
                    session_id: session_id.clone(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            ),
            (
                Topic::SessionEvents { session_id },
                ServerEvent::SessionSnapshot {
                    total: records.len() as u64,
                    total_turns: 1,
                    truncated: false,
                    from: 0,
                    end: records.len() as u64,
                    records,
                },
            ),
        ],
    );
}

pub(super) fn asked(id: &str) -> agent::AgentEvent {
    agent::AgentEvent::ItemCompleted(agent::ThreadItem {
        id: format!("{id}-ask"),
        parent_item_id: None,
        content: agent::ItemContent::UserMessage {
            // The agent's question is long, so its bubble is not where its
            // lead's is drawn over it.
            text: if id == "mirror" {
                "What is in the agent's own thread, which goes on asking? ".repeat(6)
            } else {
                format!("What is in {id}?")
            },
            attachments: vec![],
        },
    })
}

pub(super) fn started() -> agent::AgentEvent {
    agent::AgentEvent::TurnStarted {
        turn_id: "turn".into(),
    }
}

pub(super) fn completed() -> agent::AgentEvent {
    agent::AgentEvent::TurnCompleted {
        turn_id: "turn".into(),
        status: agent::TurnStatus::Completed,
        usage: None,
    }
}

/// Whether `quad` lies within `bounds`.
fn within(
    quad: &gpui::Bounds<gpui::ScaledPixels>,
    bounds: &gpui::Bounds<gpui::ScaledPixels>,
) -> bool {
    quad.origin.x >= bounds.origin.x
        && quad.origin.y >= bounds.origin.y
        && quad.origin.x + quad.size.width <= bounds.origin.x + bounds.size.width
        && quad.origin.y + quad.size.height <= bounds.origin.y + bounds.size.height
}

/// How far in what `selector` names is drawn: the fullest colour painted
/// within its bounds.
pub(super) fn fill_of(selector: &'static str, cx: &mut VisualTestContext) -> Option<f32> {
    let bounds = cx.debug_bounds(selector)?;
    cx.update(|window, _| {
        let bounds = bounds.scale(window.scale_factor());
        Some(
            window
                .painted_quads()
                .iter()
                .filter(|quad| within(&quad.bounds, &bounds))
                .filter_map(|quad| quad.background.as_solid())
                .map(|colour| colour.a)
                .fold(0., f32::max),
        )
    })
}
