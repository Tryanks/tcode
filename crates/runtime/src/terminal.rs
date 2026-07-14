//! Live terminal workspace state.

use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_TERMINALS_PER_SESSION: usize = 6;

/// `TerminalDrawer` is a shared UI entity that swaps between conversations.
/// Globally unique tab ids prevent its geometry, selection, bell, and event
/// caches from aliasing two conversations whose first local tab would both be
/// `1`.
static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSplitDirection {
    Horizontal,
    Vertical,
}

pub struct TerminalEntry {
    pub id: u64,
    pub terminal: term::Terminal,
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
    pub open: bool,
    pub height: f32,
    pub terminals: Vec<TerminalEntry>,
    pub active_id: Option<u64>,
    pub splits: Vec<TerminalSplit>,
    pub contexts: Vec<TerminalContext>,
    next_context_id: u64,
}

impl Default for TerminalWorkspace {
    fn default() -> Self {
        Self {
            open: false,
            height: 240.,
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
        self.terminals.push(TerminalEntry { id, terminal });
        self.active_id = Some(id);
        id
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
