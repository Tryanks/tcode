//! The thread's subagents and the agents it dispatched.

use gpui::{AnyElement, App, Context, SharedString, Window};

use crate::chat::ChatView;

/// Whether the card has something to show for `chat`'s thread now.
pub(super) fn present(_chat: &ChatView, _cx: &App) -> bool {
    false
}

/// The line the card's header shows beside its title, open or shut.
pub(super) fn summary(_chat: &ChatView, _cx: &App) -> Option<SharedString> {
    None
}

/// What the card shows open, laid out at the rail's width.
pub(super) fn body(
    _chat: &ChatView,
    _window: &mut Window,
    _cx: &mut Context<ChatView>,
) -> Option<AnyElement> {
    None
}
