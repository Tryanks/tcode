//! Client-side terminal handles over the replicated host grid.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

use tcode_client::HostLink;
use tcode_protocol::{
    Command, SessionStatus, Subscription, TerminalSelection, TerminalSplitStatus, Topic,
    terminal::{TerminalClipboard, TerminalDelta, TerminalFrame, TerminalMode},
};

use super::WorkspaceStore;

pub(crate) mod model;
pub(crate) use model::{HyperlinkMatch, SelectionKind, SelectionSide, TerminalModel};

/// One replicated terminal: the host's grid plus this viewer's own scroll
/// position and selection. There is no emulator here — input is encoded from
/// the replicated mode bits and sent back as bytes.
pub struct ClientTerminal {
    id: u64,
    host: HostLink,
    model: RefCell<TerminalModel>,
}

impl ClientTerminal {
    fn new(id: u64, host: HostLink) -> Self {
        Self {
            id,
            host,
            model: RefCell::new(TerminalModel::new(format!("Terminal {id}"))),
        }
    }

    pub(crate) fn model(&self) -> std::cell::Ref<'_, TerminalModel> {
        self.model.borrow()
    }

    pub(crate) fn model_mut(&self) -> std::cell::RefMut<'_, TerminalModel> {
        self.model.borrow_mut()
    }

    pub fn label(&self) -> String {
        self.model.borrow().title()
    }

    pub fn exited(&self) -> bool {
        self.model.borrow().exited()
    }

    pub fn mode(&self) -> TerminalMode {
        self.model.borrow().mode()
    }

    pub fn keyboard_mode(&self) -> tcode_protocol::terminal::KeyboardModes {
        self.model.borrow().keyboard_mode()
    }

    pub fn modify_other_keys(&self) -> Option<u8> {
        self.model.borrow().modify_other_keys()
    }

    pub fn history_size(&self) -> usize {
        self.model.borrow().history_size()
    }

    pub fn working_directory(&self) -> PathBuf {
        self.model
            .borrow()
            .working_directory()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    /// Typing returns this viewer to the live viewport and drops its selection,
    /// then sends the bytes to the host.
    pub fn write_input(&self, bytes: impl Into<Vec<u8>>) {
        self.model.borrow_mut().prepare_input();
        self.send(bytes.into());
    }

    /// Protocol replies (mouse reports, focus events) that must not disturb the
    /// viewport or selection.
    pub fn write_raw(&self, bytes: impl Into<Vec<u8>>) {
        self.send(bytes.into());
    }

    fn send(&self, bytes: Vec<u8>) {
        let _ = self.host.dispatch(Command::TerminalInput {
            terminal_id: self.id,
            bytes,
        });
    }

    pub fn resize_with_cell_size(&self, cols: usize, rows: usize, width: u32, height: u32) {
        let cols = cols.clamp(2, 1000);
        let rows = rows.clamp(2, 1000);
        let model = self.model.borrow();
        if (model.cols(), model.rows()) == (cols, rows) {
            return;
        }
        drop(model);
        let _ = self.host.dispatch(Command::ResizeTerminal {
            terminal_id: self.id,
            cols: cols as u16,
            rows: rows as u16,
            cell_width: width.min(u16::MAX as u32) as u16,
            cell_height: height.min(u16::MAX as u32) as u16,
        });
    }

    pub fn clear(&self) {
        let _ = self.host.dispatch(Command::ClearTerminal {
            terminal_id: self.id,
        });
    }

    pub fn scroll(&self, lines: i32) {
        self.model.borrow_mut().scroll(lines);
    }

    pub fn start_selection(&self, kind: SelectionKind, point: (usize, usize), side: SelectionSide) {
        self.model.borrow_mut().start_selection(kind, point, side);
    }

    pub fn update_selection(&self, point: (usize, usize), side: SelectionSide) {
        self.model.borrow_mut().update_selection(point, side);
    }

    pub fn clear_selection(&self) {
        self.model.borrow_mut().clear_selection();
    }

    pub fn select_all(&self) {
        self.model.borrow_mut().select_all();
    }

    pub fn selected_text(&self) -> Option<TerminalSelection> {
        let (line_start, line_end, text) = self.model.borrow().selected_text()?;
        Some(TerminalSelection {
            line_start,
            line_end,
            text,
        })
    }

    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<HyperlinkMatch> {
        self.model.borrow().hyperlink_at(row, col)
    }

    pub(crate) fn take_notices(&self) -> (bool, Option<TerminalClipboard>) {
        self.model.borrow_mut().take_notices()
    }

    fn apply_frame(&self, frame: TerminalFrame) {
        self.model.borrow_mut().apply_frame(frame);
    }

    fn apply_delta(&self, delta: &TerminalDelta) {
        self.model.borrow_mut().apply_delta(delta);
    }
}

pub struct TerminalEntry {
    pub id: u64,
    pub terminal: Rc<ClientTerminal>,
}

pub struct TerminalWorkspace {
    pub terminals: Vec<TerminalEntry>,
    pub active_id: Option<u64>,
    splits: Vec<TerminalSplitStatus>,
}

impl TerminalWorkspace {
    pub(super) fn from_replica(
        status: &SessionStatus,
        terminal: impl Fn(u64) -> Option<Rc<ClientTerminal>>,
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
    pub(super) fn client_terminal(&self, id: u64) -> Option<Rc<ClientTerminal>> {
        self.terminals.get(&id).cloned()
    }

    pub(super) fn apply_terminal_frame(&mut self, id: u64, frame: &TerminalFrame) {
        if let Some(terminal) = self.terminals.get(&id) {
            terminal.apply_frame(frame.clone());
        }
    }

    pub(super) fn apply_terminal_delta(&mut self, id: u64, delta: &TerminalDelta) {
        if let Some(terminal) = self.terminals.get(&id) {
            terminal.apply_delta(delta);
        }
    }

    pub(super) fn clear_terminal_topics(&mut self) {
        for id in self.terminals.keys() {
            let _ = self.host.unsubscribe(Subscription {
                topic: Topic::Terminal { terminal_id: *id },
                after: None,
            });
        }
        self.terminals.clear();
    }

    pub(super) fn sync_terminal_topics(&mut self) {
        let ids: Vec<_> = self
            .session_status_replica
            .as_ref()
            .map(|status| status.terminals.iter().map(|entry| entry.id).collect())
            .unwrap_or_default();
        self.terminals.retain(|id, _| {
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
                if let Some(terminal) = self.terminals.get(&entry.id) {
                    terminal.model_mut().set_fallback_title(entry.title.clone());
                }
            }
        }
        for id in ids {
            if let std::collections::hash_map::Entry::Vacant(entry) = self.terminals.entry(id) {
                entry.insert(Rc::new(ClientTerminal::new(id, self.host.clone())));
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
    use tcode_protocol::terminal::{
        CellWidth, TerminalCell, TerminalCursor, TerminalRow, TerminalStyle,
    };

    fn text_row(text: &str) -> TerminalRow {
        TerminalRow {
            cells: text
                .chars()
                .map(|ch| TerminalCell {
                    text: if ch == ' ' { String::new() } else { ch.into() },
                    ..TerminalCell::default()
                })
                .collect(),
            wrapped: false,
        }
    }

    fn frame(rows: &[&str], cols: u16) -> TerminalFrame {
        TerminalFrame {
            cols,
            rows: rows.len() as u16,
            styles: vec![TerminalStyle::default()],
            visible: rows.iter().map(|row| text_row(row)).collect(),
            cursor: Some(TerminalCursor {
                row: 0,
                col: 0,
                shape: Default::default(),
                blinking: false,
            }),
            ..TerminalFrame::default()
        }
    }

    fn model(rows: &[&str], cols: u16) -> TerminalModel {
        let mut model = TerminalModel::new("tab".into());
        model.apply_frame(frame(rows, cols));
        model
    }

    #[test]
    fn simple_selection_covers_the_dragged_span_and_yields_its_text() {
        let mut model = model(&["alpha beta", "gamma"], 10);
        model.start_selection(SelectionKind::Simple, (0, 0), SelectionSide::Left);
        model.update_selection((1, 4), SelectionSide::Right);
        assert!(model.is_selected(0, 0));
        assert!(model.is_selected(1, 4));
        assert!(!model.is_selected(1, 5));
        assert_eq!(
            model.selected_text(),
            Some((1, 2, "alpha beta\ngamma".to_string()))
        );
    }

    #[test]
    fn word_selection_stops_at_delimiters_and_line_selection_takes_the_row() {
        let mut model = model(&["alpha beta"], 10);
        model.start_selection(SelectionKind::Semantic, (0, 7), SelectionSide::Left);
        assert_eq!(model.selected_text(), Some((1, 1, "beta".to_string())));

        model.start_selection(SelectionKind::Lines, (0, 3), SelectionSide::Left);
        assert_eq!(
            model.selected_text(),
            Some((1, 1, "alpha beta".to_string()))
        );
    }

    #[test]
    fn wrapped_rows_join_without_a_newline() {
        let mut wrapped = frame(&["abcde", "fghij"], 5);
        wrapped.visible[0].wrapped = true;
        let mut model = TerminalModel::new("tab".into());
        model.apply_frame(wrapped);
        model.select_all();
        assert_eq!(
            model.selected_text().map(|(_, _, text)| text),
            Some("abcdefghij".to_string())
        );
    }

    #[test]
    fn plain_urls_and_osc8_links_resolve_under_the_pointer() {
        let mut model = model(&["see https://example.com/docs?q=1 now"], 40);
        let link = model.hyperlink_at(0, 12).expect("plain url");
        assert_eq!(link.url, "https://example.com/docs?q=1");
        assert_eq!(link.start, (0, 4));
        assert!(model.hyperlink_at(0, 0).is_none());

        let mut frame = frame(&["click-me"], 40);
        for cell in &mut frame.visible[0].cells {
            cell.link = Some(tcode_protocol::terminal::TerminalLink {
                uri: "https://example.com/target".into(),
                id: None,
            });
        }
        model.apply_frame(frame);
        let link = model.hyperlink_at(0, 3).expect("osc 8 link");
        assert_eq!(link.url, "https://example.com/target");
        assert_eq!((link.start, link.end), ((0, 0), (0, 7)));
    }

    #[test]
    fn wide_spacers_follow_their_partner_into_the_selection() {
        let mut frame = frame(&["ab"], 4);
        frame.visible[0].cells[0].text = "中".into();
        frame.visible[0].cells[0].width = CellWidth::Wide;
        frame.visible[0].cells[1].text = String::new();
        frame.visible[0].cells[1].width = CellWidth::Spacer;
        let mut model = TerminalModel::new("tab".into());
        model.apply_frame(frame);
        model.start_selection(SelectionKind::Simple, (0, 0), SelectionSide::Left);
        model.update_selection((0, 0), SelectionSide::Right);
        assert!(model.is_selected(0, 0));
        assert!(model.is_selected(0, 1));
        assert_eq!(
            model.selected_text().map(|(_, _, text)| text),
            Some("中".into())
        );
    }
}
