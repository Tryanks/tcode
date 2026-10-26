//! Command output screens the host renders and pushes for an open thread.

use std::rc::Rc;

use tcode_protocol::{Subscription, TerminalFrame, Topic};

use super::WorkspaceStore;

/// What a client shows for one command execution.
#[derive(Default)]
pub(crate) struct CommandOutputReplica {
    /// The width subscribed to, while the output is on screen.
    watched: Option<u16>,
    /// The latest screen, kept across width changes and while unwatched so
    /// the panel never goes blank waiting for the next one.
    frame: Option<Rc<TerminalFrame>>,
}

impl CommandOutputReplica {
    pub(crate) fn watched(&self) -> Option<u16> {
        self.watched
    }

    pub(crate) fn frame(&self) -> Option<&Rc<TerminalFrame>> {
        self.frame.as_ref()
    }
}

impl WorkspaceStore {
    pub(crate) fn command_output_for(
        &self,
        session_id: Option<&str>,
        item_id: &str,
    ) -> Option<&CommandOutputReplica> {
        self.thread(session_id)?.command_outputs.get(item_id)
    }

    /// Hold `item_id`'s output at `cols` on the open thread. The host
    /// answers once, then pushes again while the output grows.
    pub(crate) fn watch_command_output_for(
        &mut self,
        session_id: Option<&str>,
        item_id: &str,
        cols: u16,
    ) {
        let Some(session_id) = session_id else {
            return;
        };
        let host = self.host.clone();
        let Some(thread) = self.open_thread_mut(session_id) else {
            return;
        };
        let replica = thread
            .command_outputs
            .entry(item_id.to_owned())
            .or_default();
        if replica.watched == Some(cols) {
            return;
        }
        let previous = replica.watched.replace(cols);
        if let Some(previous) = previous {
            let _ = host.unsubscribe(command_output_subscription(session_id, item_id, previous));
        }
        let _ = host.subscribe(command_output_subscription(session_id, item_id, cols));
    }

    pub(crate) fn unwatch_command_output_for(&mut self, session_id: Option<&str>, item_id: &str) {
        let Some(session_id) = session_id else {
            return;
        };
        let Some(cols) = self
            .threads
            .get_mut(session_id)
            .and_then(|thread| thread.command_outputs.get_mut(item_id))
            .and_then(|replica| replica.watched.take())
        else {
            return;
        };
        let _ = self
            .host
            .unsubscribe(command_output_subscription(session_id, item_id, cols));
    }

    /// Release the thread's outputs when it closes.
    pub(super) fn clear_command_outputs(&mut self, session_id: &str) {
        let Some(thread) = self.threads.get_mut(session_id) else {
            return;
        };
        for (item_id, replica) in std::mem::take(&mut thread.command_outputs) {
            if let Some(cols) = replica.watched {
                let _ = self
                    .host
                    .unsubscribe(command_output_subscription(session_id, &item_id, cols));
            }
        }
    }

    pub(super) fn apply_command_output(
        &mut self,
        session_id: &str,
        item_id: &str,
        cols: u16,
        frame: &TerminalFrame,
    ) {
        let Some(thread) = self.open_thread_mut(session_id) else {
            return;
        };
        if let Some(replica) = thread
            .command_outputs
            .get_mut(item_id)
            .filter(|replica| replica.watched == Some(cols))
        {
            replica.frame = Some(Rc::new(frame.clone()));
        }
    }
}

fn command_output_subscription(session_id: &str, item_id: &str, cols: u16) -> Subscription {
    Subscription {
        known_index: None,
        topic: Topic::CommandOutput {
            session_id: session_id.to_owned(),
            item_id: item_id.to_owned(),
            cols,
        },
        after: None,
    }
}
