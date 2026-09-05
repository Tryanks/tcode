//! Terminal rendering handles chosen by the presence of local affordances.
use std::{ops::Deref, path::PathBuf, sync::Arc};

use tcode_client::HostLink;
use tcode_protocol::{Command, SessionStatus, Subscription, TerminalSplitStatus, Topic};
use term::{GridEmulator, GridEvent, TermEvent, TermSnapshot};

use super::WorkspaceStore;

pub enum ClientTerminal {
    #[cfg(feature = "local-host")]
    Local(Arc<term::Terminal>),
    Remote {
        id: u64,
        host: HostLink,
        grid: GridEmulator,
        events: async_channel::Receiver<TermEvent>,
    },
}

impl Deref for ClientTerminal {
    type Target = GridEmulator;
    fn deref(&self) -> &Self::Target {
        match self {
            #[cfg(feature = "local-host")]
            Self::Local(terminal) => terminal.grid(),
            Self::Remote { grid, .. } => grid,
        }
    }
}

impl ClientTerminal {
    pub(super) fn remote(id: u64, host: HostLink, cols: usize, rows: usize) -> Self {
        let grid = GridEmulator::with_size(cols, rows);
        grid.set_fallback_title(format!("Terminal {id}"));
        let source = grid.events();
        let (sender, events) = async_channel::unbounded();
        std::thread::spawn(move || {
            while let Ok(event) = source.recv_blocking() {
                let notification = match event {
                    GridEvent::Bell => TermEvent::Bell,
                    GridEvent::ClipboardStore { kind, text } => {
                        TermEvent::ClipboardStore { kind, text }
                    }
                    // The host's emulator already answers PTY protocol queries.
                    // Sending these from every subscriber would duplicate replies.
                    GridEvent::Input(_) => continue,
                    _ => TermEvent::Wakeup,
                };
                if sender.try_send(notification).is_err() {
                    break;
                }
            }
        });
        Self::Remote {
            id,
            host,
            grid,
            events,
        }
    }

    pub fn events(&self) -> async_channel::Receiver<TermEvent> {
        match self {
            #[cfg(feature = "local-host")]
            Self::Local(terminal) => terminal.events(),
            Self::Remote { events, .. } => events.clone(),
        }
    }
    pub fn label(&self) -> String {
        match self {
            #[cfg(feature = "local-host")]
            Self::Local(terminal) => terminal.label(),
            Self::Remote { grid, .. } => grid.title(),
        }
    }
    pub fn write_input(&self, bytes: impl Into<Vec<u8>>) {
        match self {
            #[cfg(feature = "local-host")]
            Self::Local(terminal) => terminal.write_input(bytes),
            Self::Remote { id, host, grid, .. } => {
                grid.prepare_input();
                let _ = host.dispatch(Command::TerminalInput {
                    terminal_id: *id,
                    bytes: bytes.into(),
                });
            }
        }
    }
    pub fn write_raw(&self, bytes: impl Into<Vec<u8>>) {
        match self {
            #[cfg(feature = "local-host")]
            Self::Local(terminal) => terminal.write_raw(bytes),
            Self::Remote { id, host, .. } => {
                let _ = host.dispatch(Command::TerminalInput {
                    terminal_id: *id,
                    bytes: bytes.into(),
                });
            }
        }
    }
    pub fn resize_with_cell_size(&self, cols: usize, rows: usize, width: u32, height: u32) {
        match self {
            #[cfg(feature = "local-host")]
            Self::Local(terminal) => terminal.resize_with_cell_size(cols, rows, width, height),
            Self::Remote { id, host, grid, .. } => {
                let cols = cols.clamp(2, 1000);
                let rows = rows.clamp(2, 1000);
                if grid.resize_with_cell_size_if_changed(cols, rows, width, height) {
                    let _ = host.dispatch(Command::ResizeTerminal {
                        terminal_id: *id,
                        cols: cols as u16,
                        rows: rows as u16,
                    });
                }
            }
        }
    }
    pub fn snapshot(&self) -> TermSnapshot {
        match self {
            #[cfg(feature = "local-host")]
            Self::Local(terminal) => terminal.snapshot(),
            Self::Remote { grid, .. } => grid.snapshot(),
        }
    }
    pub fn exited(&self) -> bool {
        match self {
            #[cfg(feature = "local-host")]
            Self::Local(terminal) => terminal.exited(),
            Self::Remote { grid, .. } => grid.exited(),
        }
    }
    pub fn working_directory(&self) -> PathBuf {
        match self {
            #[cfg(feature = "local-host")]
            Self::Local(terminal) => terminal.working_directory(),
            Self::Remote { .. } => PathBuf::new(),
        }
    }
}

pub struct TerminalEntry {
    pub id: u64,
    pub terminal: Arc<ClientTerminal>,
}

pub struct TerminalWorkspace {
    pub terminals: Vec<TerminalEntry>,
    pub active_id: Option<u64>,
    splits: Vec<TerminalSplitStatus>,
}
impl TerminalWorkspace {
    pub(super) fn from_replica(
        status: &SessionStatus,
        terminal: impl Fn(u64) -> Option<Arc<ClientTerminal>>,
    ) -> Self {
        Self {
            terminals: status
                .terminals
                .iter()
                .filter_map(|entry| {
                    terminal(entry.id).map(|terminal| TerminalEntry {
                        id: entry.id,
                        terminal,
                    })
                })
                .collect(),
            active_id: status.active_terminal_id,
            splits: status.terminal_splits.clone(),
        }
    }
    pub fn terminal(&self, id: u64) -> Option<&TerminalEntry> {
        self.terminals.iter().find(|entry| entry.id == id)
    }
    pub fn active(&self) -> Option<&TerminalEntry> {
        self.terminal(self.active_id?)
    }
    pub fn split_for(&self, id: u64) -> Option<TerminalSplitStatus> {
        self.splits
            .iter()
            .find(|split| split.first == id || split.second == id)
            .cloned()
    }
}

impl WorkspaceStore {
    pub(super) fn client_terminal(&self, id: u64) -> Option<Arc<ClientTerminal>> {
        #[cfg(feature = "local-host")]
        if let Some(registry) = &self.terminal_registry {
            return registry
                .terminal(id)
                .map(|terminal| Arc::new(ClientTerminal::Local(terminal)));
        }
        self.remote_terminals.get(&id).cloned()
    }

    pub(super) fn clear_terminal_topics(&mut self) {
        for id in self.remote_terminals.keys() {
            let _ = self.host.unsubscribe(Subscription {
                topic: Topic::Terminal { terminal_id: *id },
                after: None,
            });
        }
        self.remote_terminals.clear();
    }

    pub(super) fn sync_terminal_topics(&mut self) {
        #[cfg(feature = "local-host")]
        if self.terminal_registry.is_some() {
            return;
        }
        let ids: Vec<_> = self
            .session_status_replica
            .as_ref()
            .map(|status| status.terminals.iter().map(|entry| entry.id).collect())
            .unwrap_or_default();
        self.remote_terminals.retain(|id, _| {
            if ids.contains(id) {
                return true;
            }
            let _ = self.host.unsubscribe(Subscription {
                topic: Topic::Terminal { terminal_id: *id },
                after: None,
            });
            false
        });
        if let Some(status) = &self.session_status_replica {
            for entry in &status.terminals {
                if let Some(terminal) = self.remote_terminals.get(&entry.id) {
                    terminal.set_fallback_title(entry.title.clone());
                    if entry.exited {
                        terminal.set_exited(None);
                    }
                }
            }
        }
        for id in ids {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                self.remote_terminals.entry(id)
            {
                entry.insert(Arc::new(ClientTerminal::remote(
                    id,
                    self.host.clone(),
                    80,
                    24,
                )));
                let _ = self.host.subscribe(Subscription {
                    topic: Topic::Terminal { terminal_id: id },
                    after: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tab_status_reads_preserve_output_damage_for_the_drawer() {
        let (tx, _) = async_channel::unbounded();
        let (_, rx) = async_channel::unbounded();
        let terminal = ClientTerminal::remote(1, HostLink::new(tx, rx), 80, 24);
        terminal.snapshot();
        terminal.feed(b"remote-output");
        assert!(!terminal.exited());
        let snapshot = terminal.snapshot();
        assert!(snapshot.text().contains("remote-output"));
        assert!(snapshot.row_damage.iter().any(|dirty| *dirty));
    }
}
