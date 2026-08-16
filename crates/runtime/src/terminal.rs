//! Live terminal workspace state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub use tcode_core::ui::TerminalSplitDirection;

/// `TerminalDrawer` is a shared UI entity that swaps between conversations.
/// Globally unique tab ids prevent its geometry, selection, bell, and event
/// caches from aliasing two conversations whose first local tab would both be
/// `1`.
static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

pub struct TerminalEntry {
    pub id: u64,
    pub terminal: Arc<term::Terminal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSplit {
    pub first: u64,
    pub second: u64,
    pub direction: TerminalSplitDirection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalContext {
    pub id: u64,
    pub terminal_label: String,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

pub struct TerminalWorkspace {
    pub terminals: Vec<TerminalEntry>,
    pub active_id: Option<u64>,
    pub splits: Vec<TerminalSplit>,
    pub contexts: Vec<TerminalContext>,
    next_context_id: u64,
}

/// The one deliberate local-transport terminal affordance.
///
/// Only opaque live terminal handles cross here; tab ids, split layout,
/// selection contexts, and active-tab state travel in serialized
/// [`tcode_protocol::SessionStatus`] events. A remote transport replaces each
/// `Arc<Terminal>` with the term crate's existing raw-byte
/// `PtyHandle`/`GridEmulator` streams rather than serializing terminal bytes as
/// JSON.
#[derive(Clone, Default)]
pub struct LocalTerminalRegistry {
    handles: Arc<RwLock<HashMap<u64, Arc<term::Terminal>>>>,
}

impl LocalTerminalRegistry {
    pub(crate) fn replace_from<'a>(
        &self,
        workspaces: impl IntoIterator<Item = &'a TerminalWorkspace>,
    ) {
        let mut handles = self.handles.write().unwrap();
        handles.clear();
        for workspace in workspaces {
            for entry in &workspace.terminals {
                handles.insert(entry.id, entry.terminal.clone());
            }
        }
    }

    pub fn terminal(&self, id: u64) -> Option<Arc<term::Terminal>> {
        self.handles.read().unwrap().get(&id).cloned()
    }
}

impl Default for TerminalWorkspace {
    fn default() -> Self {
        Self {
            terminals: Vec::new(),
            active_id: None,
            splits: Vec::new(),
            contexts: Vec::new(),
            next_context_id: 1,
        }
    }
}

impl TerminalWorkspace {
    pub fn active(&self) -> Option<&TerminalEntry> {
        let id = self.active_id?;
        self.terminals.iter().find(|entry| entry.id == id)
    }

    pub fn terminal(&self, id: u64) -> Option<&TerminalEntry> {
        self.terminals.iter().find(|entry| entry.id == id)
    }

    /// Add a terminal from the temporary app compatibility consumer.
    pub fn push(&mut self, terminal: term::Terminal) -> u64 {
        let id = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
        self.terminals.push(TerminalEntry {
            id,
            terminal: Arc::new(terminal),
        });
        self.active_id = Some(id);
        id
    }

    /// Rebuild the UI's compatibility view from serialized layout metadata
    /// plus the construction-time local live-handle registry.
    pub fn from_replica(
        status: &tcode_protocol::SessionStatus,
        registry: &LocalTerminalRegistry,
    ) -> Self {
        Self {
            terminals: status
                .terminals
                .iter()
                .filter_map(|terminal| {
                    registry.terminal(terminal.id).map(|handle| TerminalEntry {
                        id: terminal.id,
                        terminal: handle,
                    })
                })
                .collect(),
            active_id: status.active_terminal_id,
            splits: status
                .terminal_splits
                .iter()
                .map(|split| TerminalSplit {
                    first: split.first,
                    second: split.second,
                    direction: split.direction,
                })
                .collect(),
            contexts: status
                .terminal_contexts
                .iter()
                .map(|context| TerminalContext {
                    id: context.id,
                    terminal_label: context.terminal_label.clone(),
                    line_start: context.line_start,
                    line_end: context.line_end,
                    text: context.text.clone(),
                })
                .collect(),
            next_context_id: 1,
        }
    }

    pub fn split_for(&self, terminal_id: u64) -> Option<TerminalSplit> {
        self.splits
            .iter()
            .copied()
            .find(|split| split.first == terminal_id || split.second == terminal_id)
    }

    pub fn add_context(&mut self, label: String, selection: term::SelectedText) {
        let id = self.next_context_id;
        self.next_context_id += 1;
        self.contexts.push(TerminalContext {
            id,
            terminal_label: label,
            line_start: selection.line_start,
            line_end: selection.line_end,
            text: selection.text,
        });
    }
}
