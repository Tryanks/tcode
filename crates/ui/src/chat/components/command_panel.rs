use std::{collections::HashMap, fmt::Write as _, sync::Arc};

use gpui::{
    AnyElement, App, ContentMask, Hsla, IntoElement as _, ParentElement as _, Rgba, Styled as _,
    Window, canvas, div, prelude::FluentBuilder as _, px,
};
use gpui_base::ElementExt as _;
use term::{GridEmulator, TermSnapshot};

use crate::highlight;
use crate::terminal_drawer::{
    TERMINAL_CELL_HEIGHT, TERMINAL_CELL_WIDTH, TerminalPalette, layout_grid, paint_terminal_grid,
};
use crate::theme::{ActiveTheme as _, HighlightTheme};

const DEFAULT_COLS: usize = 80;
const MIN_COLS: usize = 20;
const MAX_COLS: usize = 400;
const MAX_ROWS: usize = 16;
const MAX_COMMAND_ROWS: usize = 4;
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

#[derive(Clone)]
struct CommandTheme {
    foreground: Hsla,
    background: Hsla,
    highlight_theme: Arc<HighlightTheme>,
}

impl CommandTheme {
    fn matches(&self, other: &Self) -> bool {
        self.foreground == other.foreground
            && self.background == other.background
            && Arc::ptr_eq(&self.highlight_theme, &other.highlight_theme)
    }
}

struct CachedPanel {
    command: String,
    command_snapshot: TermSnapshot,
    command_rows: usize,
    command_theme: Option<CommandTheme>,
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
        let (command_snapshot, command_rows) = command_snapshot(command, cols, None);
        let output_emulator = GridEmulator::with_size(cols, MAX_ROWS - command_rows);
        let output_snapshot = output_emulator.snapshot();
        let mut panel = Self {
            command: command.to_string(),
            command_snapshot,
            command_rows,
            command_theme: None,
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
            (self.command_snapshot, self.command_rows) =
                command_snapshot(command, self.cols, self.command_theme.as_ref());
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
        (self.command_snapshot, self.command_rows) =
            command_snapshot(&self.command, cols, self.command_theme.as_ref());
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

    fn update_command_theme(&mut self, command_theme: CommandTheme) {
        if self
            .command_theme
            .as_ref()
            .is_some_and(|cached| cached.matches(&command_theme))
        {
            return;
        }
        (self.command_snapshot, self.command_rows) =
            command_snapshot(&self.command, self.cols, Some(&command_theme));
        self.command_theme = Some(command_theme);
    }

    fn render(&mut self, on_cols_change: Option<ColsChangeHandler>, cx: &App) -> AnyElement {
        let command_theme = CommandTheme {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            highlight_theme: cx.theme().highlight_theme.clone(),
        };
        self.update_command_theme(command_theme);
        let palette = TerminalPalette {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            selection: cx.theme().primary.opacity(0.28),
        };
        let command = grid_element(
            &self.command_snapshot,
            self.command_rows,
            self.cols,
            palette,
        );
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

fn command_snapshot(
    command: &str,
    cols: usize,
    command_theme: Option<&CommandTheme>,
) -> (TermSnapshot, usize) {
    let emulator = GridEmulator::with_size(cols, MAX_COMMAND_ROWS);
    let command = clamp_command(command, cols);
    if let Some(command_theme) = command_theme {
        emulator.feed(highlighted_command(&command, command_theme).as_bytes());
    } else {
        emulator.feed(command.as_bytes());
    }
    let mut snapshot = emulator.snapshot();
    let rows = trim_snapshot(&mut snapshot, MAX_COMMAND_ROWS, 1);
    (snapshot, rows)
}

fn highlighted_command(command: &str, theme: &CommandTheme) -> String {
    let highlights = highlight::highlight_source(command, "bash", &theme.highlight_theme);
    let mut highlighted = String::new();
    let mut cursor = 0;
    for (range, style) in highlights {
        if range.start > cursor {
            push_command_color(&mut highlighted, theme.foreground, theme.background);
            highlighted.push_str(&command[cursor..range.start]);
        }
        push_command_color(
            &mut highlighted,
            style.color.unwrap_or(theme.foreground),
            theme.background,
        );
        highlighted.push_str(&command[range.clone()]);
        cursor = range.end;
    }
    if cursor < command.len() {
        push_command_color(&mut highlighted, theme.foreground, theme.background);
        highlighted.push_str(&command[cursor..]);
    }
    highlighted
}

fn push_command_color(command: &mut String, foreground: Hsla, background: Hsla) {
    let (r, g, b) = faded_command_rgb(foreground, background);
    let _ = write!(command, "\x1b[38;2;{r};{g};{b}m");
}

fn faded_command_rgb(foreground: Hsla, background: Hsla) -> (u8, u8, u8) {
    let faded = Rgba::from(background).blend(Rgba::from(foreground).opacity(0.7));
    (
        (faded.r * 255.) as u8,
        (faded.g * 255.) as u8,
        (faded.b * 255.) as u8,
    )
}

fn clamp_command(command: &str, cols: usize) -> String {
    let mut result = String::new();
    let mut row = 0;
    let mut col = 0;
    let mut chars = command.chars().filter(|ch| *ch != '\r').peekable();
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
        if col == cols {
            row += 1;
            col = 0;
        }
        if row == MAX_COMMAND_ROWS {
            result.push('…');
            break;
        }
        if row + 1 == MAX_COMMAND_ROWS && col + 1 == cols && chars.peek().is_some() {
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

    fn dark_command_theme() -> CommandTheme {
        CommandTheme {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            highlight_theme: HighlightTheme::default_dark(),
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
        let mut panel = CachedPanel::new(&"x".repeat(DEFAULT_COLS * MAX_COMMAND_ROWS + 1), "");
        panel.update_command_theme(dark_command_theme());
        assert_eq!(panel.command_rows, MAX_COMMAND_ROWS);
        assert!(
            panel
                .command_snapshot
                .cell_text(MAX_COMMAND_ROWS - 1, DEFAULT_COLS - 1)
                .is_some_and(|text| text == "…")
        );
    }

    #[test]
    fn command_highlight_is_faded_truecolor_in_terminal_cells() {
        let command = "if true; then echo yes; fi";
        let command_theme = dark_command_theme();
        let clamped = clamp_command(command, DEFAULT_COLS);
        let raw_keyword_color =
            highlight::highlight_source(&clamped, "bash", &command_theme.highlight_theme)
                .into_iter()
                .find_map(|(range, style)| {
                    clamped[range]
                        .contains("if")
                        .then_some(style.color)
                        .flatten()
                })
                .expect("bash keyword highlight color");
        let expected = faded_command_rgb(raw_keyword_color, command_theme.background);

        let mut panel = CachedPanel::new(command, "");
        panel.update_command_theme(command_theme);
        let paint = layout_grid(
            &panel.command_snapshot,
            palette(),
            false,
            None,
            false,
            false,
        );
        let command = paint
            .text_runs
            .iter()
            .find(|run| run.text.contains("if"))
            .expect("highlighted command run");
        let AnsiColor::Spec(color) = command.style.fg else {
            panic!("command keyword should use truecolor foreground");
        };
        assert_eq!((color.r, color.g, color.b), expected);
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
