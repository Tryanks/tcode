use std::collections::HashMap;

use gpui::{
    AnyElement, App, ContentMask, IntoElement as _, ParentElement as _, Styled as _, StyledText,
    Window, canvas, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{ElementExt as _, h_flex};
use term::{GridEmulator, TermSnapshot};

use crate::highlight;
use crate::terminal_drawer::{
    TERMINAL_CELL_HEIGHT, TERMINAL_CELL_WIDTH, TerminalPalette, layout_grid, paint_terminal_grid,
};
use crate::theme::ActiveTheme as _;

const DEFAULT_COLS: usize = 80;
const MIN_COLS: usize = 20;
const MAX_COLS: usize = 400;
const MAX_ROWS: usize = 16;
const MAX_COMMAND_ROWS: usize = 4;
const PROMPT_COLS: usize = 2;
const OUTPUT_TAIL_BYTES: usize = 32 * 1024;

pub(crate) type ColsChangeHandler = Box<dyn Fn(&usize, &mut Window, &mut App) + 'static>;

pub(crate) struct CommandPanelCache {
    entries: HashMap<String, CachedPanel>,
}

impl CommandPanelCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn resize(&mut self, id: &str, cols: usize) -> bool {
        self.entries
            .get_mut(id)
            .is_some_and(|panel| panel.resize(cols))
    }

    pub(crate) fn render(
        &mut self,
        id: &str,
        command: &str,
        output: &str,
        on_cols_change: Option<ColsChangeHandler>,
        cx: &App,
    ) -> AnyElement {
        let panel = self
            .entries
            .entry(id.to_string())
            .or_insert_with(|| CachedPanel::new(command, output));
        panel.update(command, output);
        panel.render(on_cols_change, cx)
    }
}

struct CachedPanel {
    command: String,
    displayed_command: String,
    command_rows: usize,
    cols: usize,
    output_emulator: GridEmulator,
    output: Vec<u8>,
    fed_start: usize,
    last_fed_was_cr: bool,
    output_snapshot: TermSnapshot,
    output_rows: usize,
}

impl CachedPanel {
    fn new(command: &str, output: &str) -> Self {
        Self::with_cols(command, output, DEFAULT_COLS)
    }

    fn with_cols(command: &str, output: &str, cols: usize) -> Self {
        let (displayed_command, command_rows) = clamp_command(command, cols);
        let output_emulator = GridEmulator::with_size(cols, MAX_ROWS - command_rows);
        let output_snapshot = output_emulator.snapshot();
        let mut panel = Self {
            command: command.to_string(),
            displayed_command,
            command_rows,
            cols,
            output_emulator,
            output: Vec::new(),
            fed_start: 0,
            last_fed_was_cr: false,
            output_snapshot,
            output_rows: 0,
        };
        panel.rebuild_output(output.as_bytes());
        panel
    }

    fn update(&mut self, command: &str, output: &str) {
        if self.command != command {
            self.command = command.to_string();
            (self.displayed_command, self.command_rows) = clamp_command(command, self.cols);
            self.rebuild_output(output.as_bytes());
            return;
        }

        let bytes = output.as_bytes();
        if bytes == self.output {
            return;
        }
        let append_only = bytes.starts_with(&self.output);
        if append_only && bytes.len().saturating_sub(self.fed_start) <= OUTPUT_TAIL_BYTES {
            self.feed_output(&bytes[self.output.len()..]);
            self.output = bytes.to_vec();
            self.refresh_output_snapshot();
        } else {
            self.rebuild_output(bytes);
        }
    }

    fn resize(&mut self, cols: usize) -> bool {
        let cols = cols.clamp(MIN_COLS, MAX_COLS);
        if self.cols == cols {
            return false;
        }
        self.cols = cols;
        (self.displayed_command, self.command_rows) = clamp_command(&self.command, cols);
        let output = self.output.clone();
        self.rebuild_output(&output);
        true
    }

    fn rebuild_output(&mut self, bytes: &[u8]) {
        self.output_emulator =
            GridEmulator::with_size(self.cols, MAX_ROWS.saturating_sub(self.command_rows).max(1));
        self.fed_start = bytes.len().saturating_sub(OUTPUT_TAIL_BYTES);
        self.last_fed_was_cr = self.fed_start > 0 && bytes[self.fed_start - 1] == b'\r';
        self.feed_output(&bytes[self.fed_start..]);
        self.output = bytes.to_vec();
        self.refresh_output_snapshot();
    }

    fn feed_output(&mut self, bytes: &[u8]) {
        let mut normalized = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if byte == b'\n' && !self.last_fed_was_cr {
                normalized.push(b'\r');
            }
            normalized.push(byte);
            self.last_fed_was_cr = byte == b'\r';
        }
        self.output_emulator.feed(&normalized);
    }

    fn refresh_output_snapshot(&mut self) {
        self.output_snapshot = self.output_emulator.snapshot();
        self.output_rows = trim_snapshot(
            &mut self.output_snapshot,
            MAX_ROWS.saturating_sub(self.command_rows).max(1),
            0,
        );
    }

    fn render(&self, on_cols_change: Option<ColsChangeHandler>, cx: &App) -> AnyElement {
        let palette = TerminalPalette {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            selection: cx.theme().primary.opacity(0.28),
        };
        let highlights = highlight::highlight_source(
            &self.displayed_command,
            "bash",
            &cx.theme().highlight_theme,
        );
        let command = h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .px_1()
            .py_1()
            .bg(cx.theme().tokens.colors.muted)
            .font_family(cx.theme().mono_font_family.clone())
            .text_size(px(13.))
            .child(div().flex_none().text_color(cx.theme().primary).child("❯ "))
            .child(div().flex_1().min_w_0().whitespace_normal().child(
                StyledText::new(self.displayed_command.clone()).with_highlights(highlights),
            ));
        let output = (self.output_rows > 0)
            .then(|| grid_element(&self.output_snapshot, self.output_rows, self.cols, palette));
        let rendered_cols = self.cols;
        div()
            .w_full()
            .overflow_hidden()
            .bg(palette.background)
            .child(command)
            .children(output.map(|output| div().mt_1().child(output)))
            .when_some(on_cols_change, |panel, on_cols_change| {
                panel.on_prepaint(move |bounds, window, cx| {
                    let cols = (f32::from(bounds.size.width) / TERMINAL_CELL_WIDTH)
                        .floor()
                        .max(0.) as usize;
                    let cols = cols.clamp(MIN_COLS, MAX_COLS);
                    if cols != rendered_cols {
                        on_cols_change(&cols, window, cx);
                    }
                })
            })
            .into_any_element()
    }
}

fn grid_element(
    snapshot: &TermSnapshot,
    rows: usize,
    cols: usize,
    palette: TerminalPalette,
) -> AnyElement {
    let paint_data = layout_grid(snapshot, palette, false, None, false, false);
    canvas(
        |_bounds, _window, _cx| (),
        move |bounds, (), window, cx| {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                paint_terminal_grid(
                    bounds,
                    window,
                    cx,
                    &paint_data,
                    palette,
                    TERMINAL_CELL_WIDTH,
                    TERMINAL_CELL_HEIGHT,
                    false,
                    |_origin, _scale_factor, _window| (),
                );
            });
        },
    )
    .w(px(cols as f32 * TERMINAL_CELL_WIDTH))
    .h(px(rows as f32 * TERMINAL_CELL_HEIGHT))
    .into_any_element()
}

fn clamp_command(command: &str, cols: usize) -> (String, usize) {
    let line_cols = cols.saturating_sub(PROMPT_COLS).max(1);
    let mut lines = vec![String::new()];
    let mut truncated = false;
    let mut chars = command.chars().filter(|ch| *ch != '\r').peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' {
            if lines.len() == MAX_COMMAND_ROWS {
                truncated = chars.peek().is_some();
                break;
            }
            lines.push(String::new());
            continue;
        }
        if lines
            .last()
            .is_some_and(|line| line.chars().count() == line_cols)
        {
            if lines.len() == MAX_COMMAND_ROWS {
                truncated = true;
                break;
            }
            lines.push(String::new());
        }
        lines.last_mut().expect("command has one line").push(ch);
    }
    if truncated {
        let last = lines.last_mut().expect("command has one line");
        if last.chars().count() == line_cols {
            last.pop();
        }
        last.push('…');
    }
    let rows = lines.len();
    (lines.join("\n"), rows)
}

fn trim_snapshot(snapshot: &mut TermSnapshot, max_rows: usize, minimum: usize) -> usize {
    let rows = (0..max_rows.min(snapshot.screen_lines))
        .rfind(|&row| {
            (0..snapshot.cols).any(|col| {
                snapshot
                    .cell_text(row, col)
                    .is_some_and(|text| text.chars().any(|ch| ch != ' ' && ch != '\0'))
            })
        })
        .map_or(minimum, |row| row + 1)
        .max(minimum);
    snapshot.screen_lines = rows;
    snapshot.visible_rows.truncate(rows);
    snapshot.row_damage.truncate(rows);
    snapshot.cursor = None;
    rows
}

#[cfg(test)]
mod tests {
    use gpui::rgb;
    use term::rio_vt::config::colors::AnsiColor;

    use super::*;
    use crate::terminal_drawer::terminal_color;

    fn palette() -> TerminalPalette {
        TerminalPalette {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            selection: rgb(0x333333).into(),
        }
    }

    #[test]
    fn ansi_output_uses_terminal_green() {
        let panel = CachedPanel::new("echo green", "\x1b[32mgreen\x1b[0m");
        let paint = layout_grid(&panel.output_snapshot, palette(), false, None, false, false);
        let green = paint
            .text_runs
            .iter()
            .find(|run| run.text.contains("green"))
            .expect("green output run");
        assert_eq!(
            terminal_color(green.style.fg, palette()),
            terminal_color(AnsiColor::Indexed(2), palette())
        );
    }

    #[test]
    fn command_is_clamped_to_four_rows() {
        let panel = CachedPanel::new(
            &"x".repeat((DEFAULT_COLS - PROMPT_COLS) * MAX_COMMAND_ROWS + 1),
            "",
        );
        assert_eq!(panel.command_rows, MAX_COMMAND_ROWS);
        assert!(panel.displayed_command.ends_with('…'));
        assert_eq!(panel.displayed_command.lines().count(), MAX_COMMAND_ROWS);
    }

    #[test]
    fn trailing_blank_output_rows_are_trimmed() {
        let panel = CachedPanel::new("printf one", "one\n\n");
        assert_eq!(panel.output_rows, 1);
    }

    #[test]
    fn bare_lf_returns_output_to_first_column() {
        let panel = CachedPanel::new("cmd", "a\nb");
        assert_eq!(panel.output_snapshot.cell_text(1, 0), Some("b".to_string()));
    }

    #[test]
    fn split_crlf_is_not_double_converted() {
        let mut panel = CachedPanel::new("cmd", "a\r");
        panel.update("cmd", "a\r\nb");
        assert_eq!(panel.output_snapshot.cell_text(1, 0), Some("b".to_string()));
        assert_eq!(panel.output_rows, 2);
    }

    #[test]
    fn output_wraps_at_the_configured_column_count() {
        let output = "abcdefghijklmnopqrstuvwxy";
        let narrow = CachedPanel::with_cols("cmd", output, 20);
        let wide = CachedPanel::with_cols("cmd", output, 40);
        assert_eq!(
            narrow.output_snapshot.cell_text(1, 0),
            Some("u".to_string())
        );
        assert_eq!(narrow.output_rows, 2);
        assert_eq!(wide.output_snapshot.cell_text(0, 20), Some("u".to_string()));
        assert_eq!(wide.output_rows, 1);
    }
}
