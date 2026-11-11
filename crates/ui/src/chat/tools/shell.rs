//! The terminal window: a command line, its output, how it exited.
//!
//! A `CommandExecution` shows what its host renders. No client owns a
//! terminal emulator: the output is the screen the host renders at the width
//! the panel subscribed to, and what scrolled off it, pushed again while the
//! command runs; both parts paint as terminal grids, and the output's scrolls
//! within the window's height. A command is only ever a `CommandExecution`:
//! the adapter or importer that knows its harness's shell tool says so.

use std::{collections::HashMap, rc::Rc, sync::Arc};

use agent::{ItemContent, ItemStatus};
use gpui::{
    AnyElement, App, Bounds, ContentMask, FocusHandle, Hsla, InteractiveElement as _,
    IntoElement as _, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement as _, Pixels, Point, Rgba, ScrollEvent, Styled as _, Window, canvas, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{ElementExt as _, v_flex};
use tcode_core::session::{EntryContent, TimelineEntry};
use tcode_protocol::{
    MAX_STORED_OUTPUT_COLS,
    terminal::{CellWidth, TerminalCell, TerminalFrame, TerminalRow, TerminalStyle},
};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use super::window::{Argument, Chip, Header, Verb, first_line};
use super::{ToolCtx, copy};
use crate::chat::model::one_line_with_break_markers;
use crate::highlight;
use crate::icon::IconName;
use crate::store::terminal::{SelectionKind, SelectionSide, TerminalModel};
use crate::terminal_drawer::{
    TERMINAL_CELL_HEIGHT, TERMINAL_CELL_WIDTH, TerminalPalette, layout_grid, paint_terminal_grid,
};
use crate::theme::{ActiveTheme as _, HighlightTheme};
use crate::typography::TextRole as _;

/// A command's header: the command on one line, its line breaks marked, and
/// an exit code that is not success.
pub(in crate::chat) fn command_header(
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    status: ItemStatus,
) -> Header {
    let (preview, breaks) = one_line_with_break_markers(command);
    Header::new(IconName::SquareTerminal, Verb("run"))
        .argument(Argument::mono(preview).map(|argument| Argument {
            markers: breaks,
            ..argument
        }))
        .chip(
            exit_code
                .filter(|code| *code != 0 && status != ItemStatus::InProgress)
                .map(Chip::Exit),
        )
        .error(first_line(output))
}

pub(in crate::chat) fn command_copy_items(command: &str, output: &str) -> Vec<(String, String)> {
    let mut items = Vec::new();
    copy(&mut items, "chat.copy_command", command);
    copy(&mut items, "chat.copy_output", output);
    items
}

/// A command line an entry ran, how it stands and the code it exited with.
pub(in crate::chat) struct Ran {
    pub(in crate::chat) command: String,
    pub(in crate::chat) status: ItemStatus,
    pub(in crate::chat) exit_code: Option<i32>,
}

/// The command `entry` ran, when it is a `CommandExecution`.
pub(in crate::chat) fn ran(entry: &TimelineEntry) -> Option<Ran> {
    let EntryContent::Item(ItemContent::CommandExecution {
        command,
        exit_code,
        status,
        ..
    }) = &entry.content
    else {
        return None;
    };
    Some(Ran {
        command: command.clone(),
        status: *status,
        exit_code: *exit_code,
    })
}

/// One run of a command line, as its window's body shows it.
pub(in crate::chat) struct ShellRun<'a> {
    pub(in crate::chat) entry: &'a TimelineEntry,
    pub(in crate::chat) command: &'a str,
    /// What the command printed, as its harness returned it.
    pub(in crate::chat) output: &'a str,
    pub(in crate::chat) running: bool,
}

/// Every shell window's body: the command line and what it printed, all of
/// it, in the terminal grid, which scrolls through it inside the window's
/// height and follows its end while it grows. The host lays its screen out
/// from the whole output it holds, so there is nothing to load.
pub(in crate::chat) fn shell_body(run: ShellRun<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
    let ShellRun {
        entry,
        command,
        output,
        running,
    } = run;
    let id = entry.id.clone();
    let handlers = output_handlers(&id, ctx);
    let panel = {
        let session_id = ctx.chat.session_id.clone();
        let panel_id = id.clone();
        let on_cols_change = ctx.cx.listener(move |_this, cols: &u16, window, cx| {
            // Fires from `on_prepaint`, in the middle of laying the list
            // out. Subscribe once the frame is done.
            let cols = *cols;
            let panel_id = panel_id.clone();
            let session_id = session_id.clone();
            cx.defer_in(window, move |this, _window, cx| {
                this.workspace_store.update(cx, |store, _| {
                    store.watch_command_output_for(session_id.as_deref(), &panel_id, cols);
                });
            });
        });
        let store = ctx.chat.workspace_store.read(ctx.cx);
        let shorten = store.shortens_long_commands();
        let replica = store.command_output_for(ctx.chat.session_id.as_deref(), &id, ctx.cx);
        let watched = replica.and_then(|replica| replica.watched());
        let frame = replica.and_then(|replica| replica.frame()).cloned();
        let screen = OutputScreen {
            watched,
            frame: frame.as_ref(),
        };
        ctx.chat.command_panels.borrow_mut().render(
            entry,
            command,
            shorten,
            output,
            screen,
            Some(Box::new(on_cols_change)),
            handlers,
            ctx.cx,
        )
    };
    let cx = ctx.app();
    let silent = output.trim().is_empty() && !running;
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .debug_selector(move || format!("tool-shell-{id}"))
        .child(panel)
        .when(silent, |body| {
            body.child(
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!("tools.no_output").into_owned()),
            )
        })
        .into_any_element()
}

type Handler<E> = Box<dyn Fn(&E, &mut Window, &mut App) + 'static>;

/// What the reader does in a panel's output: the wheel scrolls it, a press
/// and a drag select in it, and Copy takes the selection.
pub(crate) struct OutputHandlers {
    wheel: Handler<ScrollEvent>,
    press: Handler<MouseDownEvent>,
    drag: Handler<MouseMoveEvent>,
    release: Handler<MouseUpEvent>,
    copy: Handler<gpui_base::input::Copy>,
}

fn output_handlers(id: &str, ctx: &mut ToolCtx<'_, '_>) -> OutputHandlers {
    let wheel = {
        let id = id.to_owned();
        ctx.cx.listener(move |this, event: &ScrollEvent, _, cx| {
            let delta = f32::from(event.delta.pixel_delta(px(TERMINAL_CELL_HEIGHT)).y);
            let moved = this.command_panels.borrow_mut().scroll(&id, delta);
            if moved {
                // The timeline stays where it is while the output scrolls.
                cx.stop_propagation();
                cx.notify();
            }
        })
    };
    let press = {
        let id = id.to_owned();
        ctx.cx
            .listener(move |this, event: &MouseDownEvent, window, cx| {
                let focus = this.command_panels.borrow_mut().press(
                    &id,
                    event.position,
                    event.modifiers.shift,
                    cx,
                );
                if let Some(focus) = focus {
                    // The keyboard's Copy reaches the panel.
                    window.focus(&focus, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
            })
    };
    let drag = {
        let id = id.to_owned();
        ctx.cx
            .listener(move |this, event: &MouseMoveEvent, _window, cx| {
                if event.pressed_button == Some(MouseButton::Left)
                    && this.command_panels.borrow_mut().drag(&id, event.position)
                {
                    cx.notify();
                }
            })
    };
    let release = {
        let id = id.to_owned();
        ctx.cx
            .listener(move |this, _: &MouseUpEvent, _window, _cx| {
                this.command_panels.borrow_mut().release(&id);
            })
    };
    let copy = {
        let id = id.to_owned();
        ctx.cx
            .listener(move |this, _: &gpui_base::input::Copy, _window, cx| {
                let text = this.command_panels.borrow().selected_text(&id);
                match text {
                    Some(text) => cx.write_to_clipboard(gpui::ClipboardItem::new_string(text)),
                    None => cx.propagate(),
                }
            })
    };
    OutputHandlers {
        wheel: Box::new(wheel),
        press: Box::new(press),
        drag: Box::new(drag),
        release: Box::new(release),
        copy: Box::new(copy),
    }
}

const DEFAULT_COLS: u16 = 80;
/// Grid rows one panel shows, command line included.
const MAX_ROWS: usize = 16;

pub(crate) type ColsChangeHandler = Box<dyn Fn(&u16, &mut Window, &mut App) + 'static>;

pub(crate) fn clamp_cols(cols: u16) -> u16 {
    cols.min(MAX_STORED_OUTPUT_COLS)
}

/// The host's screen for one command, as far as the panel has it.
#[derive(Clone, Copy, Default)]
pub(crate) struct OutputScreen<'a> {
    /// The width subscribed to, once the panel has measured itself.
    pub(crate) watched: Option<u16>,
    /// The latest screen, possibly for an earlier width.
    pub(crate) frame: Option<&'a Rc<TerminalFrame>>,
}

/// What changed about the panels since the last frame was painted.
#[derive(Default)]
pub(crate) struct PanelChanges {
    /// Panels no longer laid out: collapsed, scrolled away or gone.
    pub(crate) unmounted: Vec<String>,
    /// Panels whose height changed, with their turn.
    pub(crate) resized: Vec<(usize, String)>,
}

impl PanelChanges {
    pub(crate) fn is_empty(&self) -> bool {
        self.unmounted.is_empty() && self.resized.is_empty()
    }
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        entry: &TimelineEntry,
        command: &str,
        shorten: bool,
        output: &str,
        screen: OutputScreen<'_>,
        on_cols_change: Option<ColsChangeHandler>,
        handlers: OutputHandlers,
        cx: &App,
    ) -> AnyElement {
        let id = entry.id.as_str();
        let panel = self.entries.entry(entry.id.clone()).or_default();
        panel.turn = entry.turn;
        panel.rendered = true;
        if panel.command != command || panel.shorten != shorten {
            panel.command = command.to_string();
            panel.shorten = shorten;
            panel.command_model = None;
        }
        panel.render(id, output, screen, on_cols_change, handlers, cx)
    }

    /// Scrolls `id`'s output by `delta`, up into what scrolled off for a
    /// positive one, a row for every row's height it adds up to: whether it
    /// moved, or would have had the delta reached a row.
    pub(crate) fn scroll(&mut self, id: &str, delta: f32) -> bool {
        let Some(panel) = self.entries.get_mut(id) else {
            return false;
        };
        let Some(built) = panel.output_model.as_mut() else {
            return false;
        };
        let total = panel.scroll_remainder + delta;
        let lines = (total / TERMINAL_CELL_HEIGHT).trunc() as i32;
        panel.scroll_remainder = total - lines as f32 * TERMINAL_CELL_HEIGHT;
        let before = built.model.display_offset();
        built.model.scroll(lines);
        let history = built.lines.saturating_sub(built.rows);
        let room = if delta > 0. {
            before < history
        } else {
            before > 0
        };
        if !room {
            panel.scroll_remainder = 0.;
        }
        room || built.model.display_offset() != before
    }

    /// A press at `position` in `id`'s output starts a selection there, or
    /// extends the one it has with `extend`; the panel's focus handle, for
    /// the keyboard's Copy, when the press was in the output.
    pub(crate) fn press(
        &mut self,
        id: &str,
        position: Point<Pixels>,
        extend: bool,
        cx: &App,
    ) -> Option<FocusHandle> {
        let panel = self.entries.get_mut(id)?;
        let point = panel.cell_at(position)?;
        let built = panel.output_model.as_mut()?;
        let side = side_of(position, panel.output_bounds.get()?, point);
        if extend && built.model.has_selection() {
            built.model.update_selection(point, side);
        } else {
            built
                .model
                .start_selection(SelectionKind::Simple, point, side);
        }
        panel.selecting = true;
        Some(panel.focus.get_or_insert_with(|| cx.focus_handle()).clone())
    }

    /// The pointer dragged to `position` while a press in `id`'s output
    /// selects: whether the selection changed.
    pub(crate) fn drag(&mut self, id: &str, position: Point<Pixels>) -> bool {
        let Some(panel) = self.entries.get_mut(id).filter(|panel| panel.selecting) else {
            return false;
        };
        let Some(bounds) = panel.output_bounds.get() else {
            return false;
        };
        let point = panel.cell_clamped(position, bounds);
        let side = side_of(position, bounds, point);
        let Some(built) = panel.output_model.as_mut() else {
            return false;
        };
        built.model.update_selection(point, side);
        true
    }

    pub(crate) fn release(&mut self, id: &str) {
        if let Some(panel) = self.entries.get_mut(id) {
            panel.selecting = false;
        }
    }

    /// What is selected in `id`'s output, as text.
    pub(crate) fn selected_text(&self, id: &str) -> Option<String> {
        let built = self.entries.get(id)?.output_model.as_ref()?;
        built.model.selected_text().map(|(_, _, text)| text)
    }

    /// Call once per painted frame, after the rows were laid out.
    pub(crate) fn settle(&mut self) -> PanelChanges {
        let mut changes = PanelChanges::default();
        self.entries.retain(|id, panel| {
            if !std::mem::take(&mut panel.rendered) {
                changes.unmounted.push(id.clone());
                return false;
            }
            if std::mem::take(&mut panel.resized) {
                changes.resized.push((panel.turn, id.clone()));
            }
            true
        });
        changes
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

struct OutputModel {
    /// The screen this was built from. Held weakly so the store's screen
    /// stays its only holder and a change from the host updates it in place:
    /// a changed screen is then a new allocation, which this no longer is.
    frame: std::rc::Weak<TerminalFrame>,
    cols: u16,
    budget: usize,
    model: TerminalModel,
    /// The rows the output shows at once.
    rows: usize,
    /// Every row it holds, what scrolled off included.
    lines: usize,
}

#[derive(Default)]
struct Panel {
    turn: usize,
    rendered: bool,
    /// The panel's height glide, kept with the panel rather than in element
    /// state, so it carries on when the chat is drawn under a new parent.
    glide: crate::motion::GlideState,
    command: String,
    /// Whether a command of more than [`LONG_COMMAND_ROWS`] rows shows only
    /// its first [`SHORT_COMMAND_ROWS`], as the user's settings say.
    shorten: bool,
    /// Rendered lazily so the first paint already carries the theme colours.
    command_model: Option<(TerminalModel, usize, u16)>,
    command_theme: Option<CommandTheme>,
    output_model: Option<OutputModel>,
    shown_rows: Option<usize>,
    resized: bool,
    /// Where the output's grid was laid out, in the window.
    output_bounds: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    /// Held by the output once a press selects in it, so Copy reaches it.
    focus: Option<FocusHandle>,
    /// A press in the output is selecting as the pointer drags.
    selecting: bool,
    /// Wheel travel short of a row, kept for the next.
    scroll_remainder: f32,
}

impl Panel {
    /// The output's cell under `position`, when it is over the output.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let bounds = self.output_bounds.get()?;
        bounds
            .contains(&position)
            .then(|| self.cell_clamped(position, bounds))
    }

    /// The output's cell nearest `position`, in its screen.
    fn cell_clamped(&self, position: Point<Pixels>, bounds: Bounds<Pixels>) -> (usize, usize) {
        let rows = self
            .output_model
            .as_ref()
            .map_or(1, |built| built.rows.max(1));
        let cols = self
            .output_model
            .as_ref()
            .map_or(1, |built| built.model.cols().max(1));
        let local = position - bounds.origin;
        let row = (f32::from(local.y) / TERMINAL_CELL_HEIGHT).floor().max(0.) as usize;
        let col = (f32::from(local.x) / TERMINAL_CELL_WIDTH).floor().max(0.) as usize;
        (row.min(rows - 1), col.min(cols - 1))
    }

    fn render(
        &mut self,
        id: &str,
        output: &str,
        screen: OutputScreen<'_>,
        on_cols_change: Option<ColsChangeHandler>,
        handlers: OutputHandlers,
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
        let cols = screen
            .watched
            .or(screen.frame.map(|frame| frame.cols))
            .unwrap_or(DEFAULT_COLS);
        // The command line wraps at the same width as the output.
        if self
            .command_model
            .as_ref()
            .is_none_or(|(_, _, built)| *built != cols)
        {
            let (model, rows) = command_model(
                &self.command,
                cols,
                self.shorten,
                self.command_theme.as_ref(),
            );
            self.command_model = Some((model, rows, cols));
        }
        let (command_model, command_rows, _) = self.command_model.as_ref().expect("command model");
        let command_rows = *command_rows;
        // Rows the output may use: the panel's budget, less the command line.
        let budget = MAX_ROWS.saturating_sub(command_rows).max(1);

        let palette = TerminalPalette {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            selection: cx.theme().primary.opacity(0.28),
        };
        let command = grid_element(command_model, command_rows, cols, palette);
        let (output, output_rows) = match screen.frame {
            Some(frame) => {
                if !self.output_model.as_ref().is_some_and(|built| {
                    std::ptr::eq(built.frame.as_ptr(), Rc::as_ptr(frame)) && built.budget == budget
                }) {
                    let (mut model, rows, lines) = output_model(frame, budget);
                    // Scrolled back, the reader keeps reading what they
                    // were while more arrives; at the end, it follows.
                    if let Some(built) = &self.output_model
                        && built.model.display_offset() > 0
                    {
                        let grown = lines.saturating_sub(built.lines);
                        model.scroll((built.model.display_offset() + grown) as i32);
                    }
                    self.output_model = Some(OutputModel {
                        frame: Rc::downgrade(frame),
                        cols: frame.cols,
                        budget,
                        model,
                        rows,
                        lines,
                    });
                }
                let built = self.output_model.as_ref().expect("output model");
                (
                    grid_element(&built.model, built.rows, built.cols, palette),
                    built.rows,
                )
            }
            // Space for the rows the output is likely to need, so the screen
            // usually lands without moving what follows.
            None => {
                let rows = placeholder_rows(output, budget);
                (
                    div()
                        .w_full()
                        .h(px(rows as f32 * TERMINAL_CELL_HEIGHT))
                        .into_any_element(),
                    rows,
                )
            }
        };
        let rows = command_rows + output_rows;
        self.resized |= self.shown_rows.is_some_and(|shown| shown != rows);
        self.shown_rows = Some(rows);

        let watched = screen.watched;
        let output_id = id.to_string();
        let output_bounds = self.output_bounds.clone();
        let focus = self.focus.clone();
        // The rows held for the screen are a guess; when the host's screen
        // lands, the panel's height glides to the rows it takes.
        let landed = screen.frame.is_some() as u64;
        let panel = div()
            .w_full()
            .overflow_hidden()
            .bg(palette.background)
            .child(command)
            .when(output_rows > 0, |panel| {
                let OutputHandlers {
                    wheel,
                    press,
                    drag,
                    release,
                    copy,
                } = handlers;
                panel.child(
                    div()
                        .id(gpui::SharedString::from(format!(
                            "command-output-{output_id}"
                        )))
                        .debug_selector(move || format!("command-output-{output_id}"))
                        .overflow_hidden()
                        .cursor_text()
                        .when_some(focus, |output, focus| output.track_focus(&focus))
                        .on_prepaint(move |bounds, _, _| output_bounds.set(Some(bounds)))
                        .on_scroll(wheel)
                        .on_mouse_down(MouseButton::Left, press)
                        .on_mouse_move(drag)
                        .on_mouse_up(MouseButton::Left, release)
                        .on_action(copy)
                        .child(output),
                )
            })
            .when_some(on_cols_change, |panel, on_cols_change| {
                panel.on_prepaint(move |bounds, window, cx| {
                    let measured = clamp_cols(
                        (f32::from(bounds.size.width) / TERMINAL_CELL_WIDTH)
                            .floor()
                            .clamp(0., f32::from(u16::MAX)) as u16,
                    );
                    if watched != Some(measured) {
                        on_cols_change(&measured, window, cx);
                    }
                })
            })
            .into_any_element();
        crate::motion::SizeGlide::new(&self.glide, panel)
            .on_change(landed)
            .into_any_element()
    }
}

/// `text` as one row of cells, as far as `cols` holds it.
fn note_row(text: &str, cols: usize) -> TerminalRow {
    let mut row = TerminalRow::default();
    let mut col = 0;
    for grapheme in text.graphemes(true) {
        let width = grapheme.width().min(2);
        if width == 0 {
            continue;
        }
        if col + width > cols {
            break;
        }
        row.cells.push(TerminalCell {
            text: grapheme.to_string(),
            width: if width == 2 {
                CellWidth::Wide
            } else {
                CellWidth::Narrow
            },
            ..TerminalCell::default()
        });
        if width == 2 {
            row.cells.push(TerminalCell {
                width: CellWidth::Spacer,
                ..TerminalCell::default()
            });
        }
        col += width;
    }
    row
}

/// Which half of its cell `position` is over, as a terminal selects.
fn side_of(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    (_, col): (usize, usize),
) -> SelectionSide {
    let into = f32::from(position.x - bounds.origin.x) - col as f32 * TERMINAL_CELL_WIDTH;
    if into < TERMINAL_CELL_WIDTH / 2. {
        SelectionSide::Left
    } else {
        SelectionSide::Right
    }
}

/// The host renders a screen and what scrolled off it; the panel shows the
/// last `budget` rows of all of it, and the rest above them to scroll back
/// through: the rows it shows, and every row it holds.
fn output_model(frame: &TerminalFrame, budget: usize) -> (TerminalModel, usize, usize) {
    let mut frame = frame.clone();
    let mut lines = std::mem::take(&mut frame.history);
    // What the frame leaves out is said where it would be, first, and how to
    // have all of it.
    if frame.lines_evicted > 0 {
        let note = crate::tr!("tools.lines_left_out", count = frame.lines_evicted);
        lines.insert(0, note_row(&note, usize::from(frame.cols)));
    }
    lines.append(&mut frame.visible);
    let total = lines.len();
    let rows = total.min(budget);
    frame.visible = lines.split_off(total - rows);
    frame.history = lines;
    frame.rows = rows.min(u16::MAX as usize) as u16;
    let mut model = TerminalModel::new(String::new());
    model.apply_frame(frame);
    (model, rows, total)
}

/// Roughly the rows the host's screen of `output` will take: one per line of
/// its tail.
fn placeholder_rows(output: &str, budget: usize) -> usize {
    let trimmed = output.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return 0;
    }
    trimmed.rsplit('\n').take(budget).count()
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
    shorten: bool,
    command_theme: Option<&CommandTheme>,
) -> (TerminalModel, usize) {
    let cols = cols.max(2);
    let command = command.replace('\r', "");
    let command = if shorten {
        shortened_command(&command, cols)
    } else {
        command
    };
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

/// A command longer than this many rows is shortened.
const LONG_COMMAND_ROWS: usize = 5;
/// The rows a shortened command keeps, the last ending in an ellipsis.
const SHORT_COMMAND_ROWS: usize = 4;

/// `command` as the panel shows it at `cols` when long commands are
/// shortened: whole up to [`LONG_COMMAND_ROWS`] rows, else its first
/// [`SHORT_COMMAND_ROWS`] with an ellipsis ending the last. The maintainer's
/// rule: one over five rows is cut to four, any other is shown whole.
fn shortened_command(command: &str, cols: u16) -> String {
    let cols = usize::from(cols.max(2));
    // Where each row after the first starts, as the grid wraps the command.
    let mut row_starts = Vec::new();
    let mut col = 0;
    for (offset, text) in command.grapheme_indices(true) {
        if text == "\n" {
            row_starts.push(offset + text.len());
            col = 0;
            continue;
        }
        let width = text.width().min(2);
        if col + width > cols {
            row_starts.push(offset);
            col = 0;
        }
        col += width;
    }
    if row_starts.len() < LONG_COMMAND_ROWS {
        return command.to_owned();
    }
    // The kept rows end where the next would start; the last gives up its
    // final cell to the ellipsis when it is full.
    let last_start = row_starts[SHORT_COMMAND_ROWS - 2];
    let end = row_starts[SHORT_COMMAND_ROWS - 1];
    let mut kept = command[..end].trim_end_matches('\n').to_owned();
    let last_width: usize = kept[last_start.min(kept.len())..]
        .graphemes(true)
        .map(|text| text.width().min(2))
        .sum();
    if last_width + 1 > cols
        && let Some((offset, _)) = kept.grapheme_indices(true).next_back()
    {
        kept.truncate(offset);
    }
    kept.push('…');
    kept
}
