use std::collections::HashMap;

use gpui::{
    AnyElement, App, ContentMask, IntoElement as _, ParentElement as _, Styled as _, canvas, div,
    px,
};
use term::{GridEmulator, TermSnapshot};

use crate::terminal_drawer::{
    TERMINAL_CELL_HEIGHT, TERMINAL_CELL_WIDTH, TerminalPalette, layout_grid, paint_terminal_grid,
};
use crate::theme::ActiveTheme as _;

const COLS: usize = 80;
const MAX_ROWS: usize = 16;
const MAX_COMMAND_ROWS: usize = 4;
const OUTPUT_TAIL_BYTES: usize = 32 * 1024;

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

    pub(crate) fn render(&mut self, id: &str, command: &str, output: &str, cx: &App) -> AnyElement {
        let panel = self
            .entries
            .entry(id.to_string())
            .or_insert_with(|| CachedPanel::new(command, output));
        panel.update(command, output);
        panel.render(cx)
    }
}

struct CachedPanel {
    command: String,
    command_snapshot: TermSnapshot,
    command_rows: usize,
    output_emulator: GridEmulator,
    output: Vec<u8>,
    fed_start: usize,
    output_snapshot: TermSnapshot,
    output_rows: usize,
}

impl CachedPanel {
    fn new(command: &str, output: &str) -> Self {
        let (command_snapshot, command_rows) = command_snapshot(command);
        let output_emulator = GridEmulator::with_size(COLS, MAX_ROWS - command_rows);
        let output_snapshot = output_emulator.snapshot();
        let mut panel = Self {
            command: command.to_string(),
            command_snapshot,
            command_rows,
            output_emulator,
            output: Vec::new(),
            fed_start: 0,
            output_snapshot,
            output_rows: 0,
        };
        panel.rebuild_output(output.as_bytes());
        panel
    }

    fn update(&mut self, command: &str, output: &str) {
        if self.command != command {
            self.command = command.to_string();
            (self.command_snapshot, self.command_rows) = command_snapshot(command);
            self.rebuild_output(output.as_bytes());
            return;
        }

        let bytes = output.as_bytes();
        if bytes == self.output {
            return;
        }
        let append_only = bytes.starts_with(&self.output);
        if append_only && bytes.len().saturating_sub(self.fed_start) <= OUTPUT_TAIL_BYTES {
            self.output_emulator.feed(&bytes[self.output.len()..]);
            self.output = bytes.to_vec();
            self.refresh_output_snapshot();
        } else {
            self.rebuild_output(bytes);
        }
    }

    fn rebuild_output(&mut self, bytes: &[u8]) {
        self.output_emulator = GridEmulator::with_size(COLS, MAX_ROWS - self.command_rows);
        self.fed_start = bytes.len().saturating_sub(OUTPUT_TAIL_BYTES);
        self.output_emulator.feed(&bytes[self.fed_start..]);
        self.output = bytes.to_vec();
        self.refresh_output_snapshot();
    }

    fn refresh_output_snapshot(&mut self) {
        self.output_snapshot = self.output_emulator.snapshot();
        self.output_rows =
            trim_snapshot(&mut self.output_snapshot, MAX_ROWS - self.command_rows, 0);
    }

    fn render(&self, cx: &App) -> AnyElement {
        let palette = TerminalPalette {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            selection: cx.theme().primary.opacity(0.28),
        };
        let command = grid_element(&self.command_snapshot, self.command_rows, palette);
        let output = (self.output_rows > 0)
            .then(|| grid_element(&self.output_snapshot, self.output_rows, palette));
        div()
            .w(px(COLS as f32 * TERMINAL_CELL_WIDTH))
            .max_w_full()
            .overflow_hidden()
            .bg(palette.background)
            .child(command)
            .children(output)
            .into_any_element()
    }
}

fn grid_element(snapshot: &TermSnapshot, rows: usize, palette: TerminalPalette) -> AnyElement {
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
    .w(px(COLS as f32 * TERMINAL_CELL_WIDTH))
    .h(px(rows as f32 * TERMINAL_CELL_HEIGHT))
    .into_any_element()
}

fn command_snapshot(command: &str) -> (TermSnapshot, usize) {
    let emulator = GridEmulator::with_size(COLS, MAX_COMMAND_ROWS);
    emulator.feed(clamp_command(command).as_bytes());
    let mut snapshot = emulator.snapshot();
    let rows = trim_snapshot(&mut snapshot, MAX_COMMAND_ROWS, 1);
    (snapshot, rows)
}

fn clamp_command(command: &str) -> String {
    let mut result = String::new();
    let mut row = 0;
    let mut col = 0;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' {
            if row + 1 == MAX_COMMAND_ROWS && chars.peek().is_some() {
                result.push('…');
                break;
            }
            result.push_str("\r\n");
            row += 1;
            col = 0;
            continue;
        }
        if ch == '\r' {
            continue;
        }
        if col == COLS {
            row += 1;
            col = 0;
        }
        if row == MAX_COMMAND_ROWS {
            result.push('…');
            break;
        }
        if row + 1 == MAX_COMMAND_ROWS && col + 1 == COLS && chars.peek().is_some() {
            result.push('…');
            break;
        }
        result.push(ch);
        col += 1;
    }
    result
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
        let panel = CachedPanel::new(&"x".repeat(COLS * MAX_COMMAND_ROWS + 1), "");
        assert_eq!(panel.command_rows, MAX_COMMAND_ROWS);
        assert!(
            panel
                .command_snapshot
                .cell_text(MAX_COMMAND_ROWS - 1, COLS - 1)
                .is_some_and(|text| text == "…")
        );
    }

    #[test]
    fn trailing_blank_output_rows_are_trimmed() {
        let panel = CachedPanel::new("printf one", "one\n\n");
        assert_eq!(panel.output_rows, 1);
    }
}
