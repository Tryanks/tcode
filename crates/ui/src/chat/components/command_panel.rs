//! The expanded command block: the command line, then its captured output.
//!
//! No client owns a terminal emulator, so the output grid is the host's answer
//! to "render this stored item at N columns". Until that answer arrives the
//! panel shows the raw text, which is always something rather than a gap.

use std::{collections::HashMap, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, App, ContentMask, Hsla, IntoElement as _, ParentElement as _, Rgba, Styled as _,
    Task, Window, canvas, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{ElementExt as _, v_flex};
use tcode_protocol::{
    STORED_OUTPUT_COLS,
    terminal::{CellWidth, TerminalCell, TerminalFrame, TerminalRow, TerminalStyle},
};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::highlight;
use crate::store::terminal::TerminalModel;
use crate::terminal_drawer::{
    TERMINAL_CELL_HEIGHT, TERMINAL_CELL_WIDTH, TERMINAL_FONT_FAMILY, TERMINAL_FONT_SIZE,
    TerminalPalette, layout_grid, paint_terminal_grid,
};
use crate::theme::{ActiveTheme as _, HighlightTheme};

const DEFAULT_COLS: u16 = 80;
/// Grid rows one panel shows, command line included.
const MAX_ROWS: usize = 16;
const MAX_COMMAND_ROWS: usize = 4;
/// Stored-output renders the host may be working on at once. Scrolling a long
/// thread expands many panels; without a ceiling every one of them would ask.
const MAX_IN_FLIGHT: usize = 4;

pub(crate) type ColsChangeHandler = Box<dyn Fn(&u16, &mut Window, &mut App) + 'static>;

pub(crate) fn clamp_cols(cols: u16) -> u16 {
    cols.clamp(*STORED_OUTPUT_COLS.start(), *STORED_OUTPUT_COLS.end())
}

pub(crate) struct CommandPanelCache {
    entries: HashMap<String, Panel>,
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

    pub(crate) fn render(
        &mut self,
        id: &str,
        command: &str,
        output: &str,
        on_cols_change: Option<ColsChangeHandler>,
        cx: &App,
    ) -> AnyElement {
        let panel = self.entries.entry(id.to_string()).or_default();
        panel.update(command, output);
        panel.render(output, on_cols_change, cx)
    }

    /// Note the width `id` was laid out at and claim a request slot.
    ///
    /// `Some(generation)` means the caller must ask the host and hand the
    /// answer back to [`Self::adopt`] under that generation.
    pub(crate) fn claim(&mut self, id: &str, cols: u16) -> Option<u64> {
        let in_flight = self
            .entries
            .values()
            .filter(|panel| panel.in_flight)
            .count();
        let panel = self.entries.get_mut(id)?;
        if panel.cols != cols {
            panel.cols = cols;
            panel.generation += 1;
            // The command line wraps at the same width as the output.
            panel.command_model = None;
        }
        if panel.frames.contains_key(&cols) || panel.requested == Some(panel.generation) {
            return None;
        }
        if in_flight >= MAX_IN_FLIGHT {
            return None;
        }
        panel.requested = Some(panel.generation);
        panel.in_flight = true;
        Some(panel.generation)
    }

    /// Keep the debounce timer alive for as long as it is the current one.
    pub(crate) fn hold(&mut self, id: &str, task: Task<()>) {
        if let Some(panel) = self.entries.get_mut(id) {
            panel.task = Some(task);
        }
    }

    /// Take the host's answer. A frame for a width the panel has since left is
    /// dropped: `generation` moved on without it.
    pub(crate) fn adopt(
        &mut self,
        id: &str,
        generation: u64,
        frame: Option<TerminalFrame>,
    ) -> bool {
        let Some(panel) = self.entries.get_mut(id) else {
            return false;
        };
        panel.in_flight = false;
        if panel.generation != generation {
            return false;
        }
        let Some(frame) = frame else {
            return false;
        };
        panel.frames.insert(frame.cols, Rc::new(frame));
        true
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

struct Panel {
    command: String,
    /// Rendered lazily so the first paint already carries the theme colours.
    command_model: Option<(TerminalModel, usize)>,
    command_rows: usize,
    command_theme: Option<CommandTheme>,
    cols: u16,
    /// `output.len()` the cached frames describe. A running command's output
    /// grows, which retires them.
    version: usize,
    frames: HashMap<u16, Rc<TerminalFrame>>,
    /// Bumped whenever the frames on hand stop describing what is on screen.
    generation: u64,
    /// The generation the host has already been asked about.
    requested: Option<u64>,
    in_flight: bool,
    task: Option<Task<()>>,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            command: String::new(),
            command_model: None,
            command_rows: 0,
            command_theme: None,
            cols: DEFAULT_COLS,
            version: usize::MAX,
            frames: HashMap::new(),
            generation: 0,
            requested: None,
            in_flight: false,
            task: None,
        }
    }
}

impl Panel {
    fn update(&mut self, command: &str, output: &str) {
        if self.command != command {
            self.command = command.to_string();
            self.command_model = None;
        }
        if self.version != output.len() {
            self.version = output.len();
            self.frames.clear();
            self.generation += 1;
        }
    }

    /// Rows the output may use: the panel's budget, less the command line.
    fn output_budget(&self) -> usize {
        MAX_ROWS.saturating_sub(self.command_rows).max(1)
    }

    fn render(
        &mut self,
        output: &str,
        on_cols_change: Option<ColsChangeHandler>,
        cx: &App,
    ) -> AnyElement {
        let theme = CommandTheme {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            highlight_theme: cx.theme().highlight_theme.clone(),
        };
        if !self
            .command_theme
            .as_ref()
            .is_some_and(|cached| cached.matches(&theme))
        {
            self.command_model = None;
            self.command_theme = Some(theme);
        }
        let cols = self.cols;
        if self.command_model.is_none() {
            let theme = self.command_theme.clone();
            self.command_model = Some(command_model(&self.command, cols, theme.as_ref()));
        }
        let (command_model, command_rows) = self.command_model.as_ref().expect("command model");
        self.command_rows = *command_rows;

        let palette = TerminalPalette {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            selection: cx.theme().primary.opacity(0.28),
        };
        let command = grid_element(command_model, *command_rows, cols, palette);
        let budget = self.output_budget();
        let output = match self.frames.get(&cols) {
            Some(frame) => {
                let (model, rows) = output_model(frame, budget);
                (rows > 0).then(|| grid_element(&model, rows, cols, palette))
            }
            None => plain_output(output, budget),
        };
        // The host has not answered for this width yet, so keep asking even
        // when the measured width itself did not move.
        let awaiting = self.requested != Some(self.generation);
        div()
            .w_full()
            .overflow_hidden()
            .bg(palette.background)
            .child(command)
            .children(output)
            .when_some(on_cols_change, |panel, on_cols_change| {
                panel.on_prepaint(move |bounds, window, cx| {
                    let measured = clamp_cols(
                        (f32::from(bounds.size.width) / TERMINAL_CELL_WIDTH)
                            .floor()
                            .clamp(0., f32::from(u16::MAX)) as u16,
                    );
                    if measured != cols || awaiting {
                        on_cols_change(&measured, window, cx);
                    }
                })
            })
            .into_any_element()
    }
}

/// The host renders a full screen; the panel shows its last `budget` rows.
fn output_model(frame: &TerminalFrame, budget: usize) -> (TerminalModel, usize) {
    let mut frame = frame.clone();
    let overflow = frame.visible.len().saturating_sub(budget);
    frame.visible.drain(..overflow);
    frame.rows = frame.visible.len().min(u16::MAX as usize) as u16;
    let rows = frame.visible.len();
    let mut model = TerminalModel::new(String::new());
    model.apply_frame(frame);
    (model, rows)
}

/// What the panel shows before the host answers, and on every client while a
/// command is still producing output.
fn plain_output(output: &str, budget: usize) -> Option<AnyElement> {
    let lines = output.lines().collect::<Vec<_>>();
    let tail = &lines[lines.len().saturating_sub(budget)..];
    (!tail.is_empty()).then(|| {
        v_flex()
            .w_full()
            .overflow_hidden()
            .font_family(TERMINAL_FONT_FAMILY)
            .text_size(px(TERMINAL_FONT_SIZE))
            .line_height(px(TERMINAL_CELL_HEIGHT))
            .children(
                tail.iter()
                    .map(|line| div().child(line.to_string()).into_any_element()),
            )
            .into_any_element()
    })
}

fn grid_element(
    model: &TerminalModel,
    rows: usize,
    cols: u16,
    palette: TerminalPalette,
) -> AnyElement {
    let paint_data = layout_grid(model, palette, false, None, false, false);
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
                );
            });
        },
    )
    .w(px(f32::from(cols) * TERMINAL_CELL_WIDTH))
    .h(px(rows as f32 * TERMINAL_CELL_HEIGHT))
    .into_any_element()
}

/// Lay the command out into grid cells so it shares the output's metrics.
///
/// The command is the client's own text, not captured terminal output: it
/// carries no escape sequences, and its colours come from this client's syntax
/// theme, which the host has no business knowing.
fn command_model(
    command: &str,
    cols: u16,
    command_theme: Option<&CommandTheme>,
) -> (TerminalModel, usize) {
    let cols = cols.max(2);
    let command = clamp_command(command, cols);
    let highlights = command_theme
        .map(|theme| highlight::highlight_source(&command, "bash", &theme.highlight_theme))
        .unwrap_or_default();

    let mut styles = vec![TerminalStyle::default()];
    let mut visible = vec![TerminalRow::default()];
    let mut col = 0usize;
    for (offset, text) in command.grapheme_indices(true) {
        if text == "\n" {
            visible.push(TerminalRow::default());
            col = 0;
            continue;
        }
        let width = text.width().min(2);
        if width == 0 {
            continue;
        }
        if col + width > usize::from(cols) {
            visible.push(TerminalRow::default());
            col = 0;
        }
        let style = command_theme.map_or_else(TerminalStyle::default, |theme| {
            let color = highlights
                .iter()
                .find(|(range, _)| range.contains(&offset))
                .and_then(|(_, style)| style.color)
                .unwrap_or(theme.foreground);
            faded_command_style(color, theme.background)
        });
        let style = match styles.iter().position(|existing| *existing == style) {
            Some(index) => index as u16,
            None => {
                styles.push(style);
                (styles.len() - 1) as u16
            }
        };
        let cells = &mut visible.last_mut().expect("a row is always open").cells;
        cells.push(TerminalCell {
            text: text.to_string(),
            style,
            width: if width == 2 {
                CellWidth::Wide
            } else {
                CellWidth::Narrow
            },
            ..TerminalCell::default()
        });
        if width == 2 {
            cells.push(TerminalCell {
                style,
                width: CellWidth::Spacer,
                ..TerminalCell::default()
            });
        }
        col += width;
    }

    let rows = visible.len().max(1);
    let mut model = TerminalModel::new(String::new());
    model.apply_frame(TerminalFrame {
        cols,
        rows: rows.min(u16::MAX as usize) as u16,
        styles,
        visible,
        ..TerminalFrame::default()
    });
    (model, rows)
}

/// The command reads as context, not as output, so it sits at 70% contrast.
fn faded_command_style(foreground: Hsla, background: Hsla) -> TerminalStyle {
    let (r, g, b) = faded_command_rgb(foreground, background);
    TerminalStyle {
        fg: tcode_protocol::terminal::TerminalColor::Rgb { r, g, b },
        ..TerminalStyle::default()
    }
}

fn faded_command_rgb(foreground: Hsla, background: Hsla) -> (u8, u8, u8) {
    let faded = Rgba::from(background).blend(Rgba::from(foreground).opacity(0.7));
    (
        (faded.r * 255.) as u8,
        (faded.g * 255.) as u8,
        (faded.b * 255.) as u8,
    )
}

fn clamp_command(command: &str, cols: u16) -> String {
    let cols = usize::from(cols.max(2));
    let command = command.replace('\r', "");
    let mut result = String::new();
    let mut row = 0;
    let mut col = 0;
    for text in command.graphemes(true) {
        let width = text.width().min(2);
        let next_row = text == "\n" || col + width > cols;
        if next_row && row + 1 == MAX_COMMAND_ROWS {
            // Make room on the last visible row without splitting a grapheme
            // or putting the ellipsis onto a fifth row.
            if col == cols {
                let (offset, _) = result.grapheme_indices(true).next_back().unwrap();
                result.truncate(offset);
            }
            result.push('…');
            break;
        }
        if next_row {
            row += 1;
            col = 0;
        }
        result.push_str(text);
        if text != "\n" {
            col += width;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use gpui::rgb;
    use tcode_protocol::terminal::TerminalColor;

    use super::*;

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

    fn frame(cols: u16, lines: &[&str]) -> TerminalFrame {
        TerminalFrame {
            cols,
            rows: lines.len() as u16,
            visible: lines
                .iter()
                .map(|line| TerminalRow {
                    cells: line
                        .chars()
                        .map(|ch| TerminalCell {
                            text: ch.to_string(),
                            ..TerminalCell::default()
                        })
                        .collect(),
                    wrapped: false,
                })
                .collect(),
            ..TerminalFrame::default()
        }
    }

    /// The panel adopts the frame it is currently asking for, ignores one for a
    /// width it has already left, and asks again after the width moves.
    #[test]
    fn frames_are_adopted_for_the_current_width_only() {
        let mut cache = CommandPanelCache::new();
        cache.entries.insert("item".into(), Panel::default());

        let first = cache.claim("item", 40).expect("first request");
        assert_eq!(cache.claim("item", 40), None, "the width is already asked");

        let second = cache.claim("item", 120).expect("the width moved");
        assert_ne!(first, second);
        assert!(!cache.adopt("item", first, Some(frame(40, &["stale"]))));
        assert!(cache.adopt("item", second, Some(frame(120, &["fresh"]))));

        let panel = &cache.entries["item"];
        assert!(!panel.frames.contains_key(&40));
        assert!(panel.frames.contains_key(&120));
        // A width with a frame on hand is never re-requested.
        assert_eq!(cache.claim("item", 120), None);
    }

    #[test]
    fn concurrent_requests_are_bounded() {
        let mut cache = CommandPanelCache::new();
        for index in 0..MAX_IN_FLIGHT + 1 {
            cache
                .entries
                .insert(format!("item-{index}"), Panel::default());
        }
        let claimed = (0..MAX_IN_FLIGHT + 1)
            .filter(|index| cache.claim(&format!("item-{index}"), 40).is_some())
            .count();
        assert_eq!(claimed, MAX_IN_FLIGHT);

        // An answer frees the slot the next panel was waiting for.
        cache.adopt("item-0", 0, None);
        assert!(cache.claim(&format!("item-{MAX_IN_FLIGHT}"), 40).is_some());
    }

    #[test]
    fn growing_output_retires_the_cached_frames() {
        let mut cache = CommandPanelCache::new();
        cache.entries.insert("item".into(), Panel::default());
        let generation = cache.claim("item", 40).expect("request");
        assert!(cache.adopt("item", generation, Some(frame(40, &["one"]))));

        cache
            .entries
            .get_mut("item")
            .expect("panel")
            .update("cmd", "one\ntwo\n");
        assert!(
            cache.claim("item", 40).is_some(),
            "longer output must be re-rendered"
        );
    }

    #[test]
    fn command_is_clamped_to_four_rows() {
        let command = "x".repeat(usize::from(DEFAULT_COLS) * MAX_COMMAND_ROWS + 1);
        let (model, rows) = command_model(&command, DEFAULT_COLS, Some(&dark_command_theme()));
        assert_eq!(rows, MAX_COMMAND_ROWS);
        assert!(
            model
                .cell(MAX_COMMAND_ROWS - 1, usize::from(DEFAULT_COLS) - 1)
                .is_some_and(|cell| cell.text == "…")
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

        let (model, rows) = command_model(command, DEFAULT_COLS, Some(&command_theme));
        let paint = layout_grid(&model, palette(), false, None, false, false);
        assert_eq!(rows, 1);
        let run = paint
            .text_runs
            .iter()
            .find(|run| run.text.contains("if"))
            .expect("highlighted command run");
        let TerminalColor::Rgb { r, g, b } = run.style.fg else {
            panic!("command keyword should use truecolor foreground");
        };
        assert_eq!((r, g, b), expected);
    }

    #[test]
    fn command_cjk_uses_two_columns_without_overlapping_following_text() {
        let (model, rows) = command_model("a中文b", 8, Some(&dark_command_theme()));
        assert_eq!(rows, 1);
        for (col, text, width) in [
            (0, "a", CellWidth::Narrow),
            (1, "中", CellWidth::Wide),
            (2, "", CellWidth::Spacer),
            (3, "文", CellWidth::Wide),
            (4, "", CellWidth::Spacer),
            (5, "b", CellWidth::Narrow),
        ] {
            let cell = model.cell(0, col).expect("command cell");
            assert_eq!((cell.text.as_str(), cell.width), (text, width));
        }
        let paint = layout_grid(&model, palette(), false, None, false, false);
        assert!(paint.text_runs.iter().any(|run| run.start_col == 3));
        assert!(paint.text_runs.iter().any(|run| run.start_col == 5));
    }

    #[test]
    fn command_wraps_whole_graphemes_and_keeps_ellipsis_in_four_rows() {
        let (model, rows) = command_model("abc中文e\u{301}👩‍💻z", 4, None);
        assert_eq!(rows, 3);
        for (row, col, text) in [
            (0, 2, "c"),
            (1, 0, "中"),
            (1, 2, "文"),
            (2, 0, "e\u{301}"),
            (2, 1, "👩‍💻"),
            (2, 3, "z"),
        ] {
            assert_eq!(model.cell(row, col).unwrap().text, text);
        }

        for command in ["中文".repeat(5), "中文\n".repeat(5)] {
            let (model, rows) = command_model(&command, 4, None);
            assert_eq!(rows, MAX_COMMAND_ROWS);
            assert_eq!(model.cell(3, 0).unwrap().text, "中");
            assert_eq!(model.cell(3, 2).unwrap().text, "…");
            assert!(model.cell(3, 3).is_none_or(|cell| cell.text.is_empty()));
        }
    }

    #[test]
    fn command_wraps_at_the_measured_width() {
        let (model, rows) = command_model("abcdefghij", 5, None);
        assert_eq!(rows, 2);
        assert_eq!(
            model.cell(1, 0).map(|cell| cell.text.clone()),
            Some("f".into())
        );
    }

    /// Long output keeps its tail: the panel is a window onto the end of a run.
    #[test]
    fn output_shows_the_last_rows_that_fit() {
        let lines = ["one", "two", "three", "four"];
        let (model, rows) = output_model(&frame(40, &lines), 2);
        assert_eq!(rows, 2);
        assert_eq!(
            model.cell(0, 0).map(|cell| cell.text.clone()),
            Some("t".into())
        );
    }
}
