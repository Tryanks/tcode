//! Small, UI-agnostic terminal runtime built on Alacritty's terminal core.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener, Notify as _, WindowSize},
    event_loop::{EventLoop, Msg, Notifier},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{Config, TermMode, cell::Flags},
    tty,
    vte::ansi::{Color as AlacrittyColor, NamedColor, Rgb},
};

mod hyperlinks;
pub mod mappings;
mod pty_info;
pub use hyperlinks::HyperlinkMatch;

const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    DefaultForeground,
    DefaultBackground,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// The cell's primary character followed by any combining characters.
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub selected: bool,
    /// Whether this cell contains a glyph which occupies two grid columns.
    pub wide: bool,
    /// Whether this cell is the second-column placeholder for a wide glyph.
    pub wide_spacer: bool,
}

/// A rendering-relevant event emitted by the terminal backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermEvent {
    /// The terminal grid or cursor changed.
    Wakeup,
    /// The terminal title changed.
    TitleChanged,
    /// The child changed whether its cursor should blink.
    CursorBlinkingChanged,
    /// The child rang the terminal bell.
    Bell,
    /// The child process exited.
    Exited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedText {
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct TermState {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    pub cursor: Option<(usize, usize)>,
    pub cursor_shape: CursorShape,
    pub cursor_blinking: bool,
    pub title: String,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub display_offset: usize,
    pub history_size: usize,
    pub mode: ModeSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
    HollowBlock,
    Hidden,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeSnapshot {
    pub mouse_click: bool,
    pub mouse_motion: bool,
    pub mouse_drag: bool,
    pub sgr_mouse: bool,
    pub utf8_mouse: bool,
    pub alt_screen: bool,
    pub alternate_scroll: bool,
    pub bracketed_paste: bool,
    pub app_cursor: bool,
    pub focus_in_out: bool,
}

impl ModeSnapshot {
    pub fn mouse_mode(self) -> bool {
        self.mouse_click || self.mouse_motion || self.mouse_drag
    }

    pub fn routes_mouse(self, shift: bool) -> bool {
        self.mouse_mode() && !shift
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Semantic,
    Lines,
}

impl TermState {
    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        self.cells.get(row.checked_mul(self.cols)? + col)
    }

    pub fn text(&self) -> String {
        self.cells
            .chunks(self.cols)
            .map(|row| {
                let text = row.iter().filter(|cell| !cell.wide_spacer).fold(
                    String::new(),
                    |mut text, cell| {
                        text.push_str(&cell.text);
                        text
                    },
                );
                text.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

impl Size {
    fn window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.rows as u16,
            num_cols: self.cols as u16,
            cell_width: 8,
            cell_height: 17,
        }
    }
}

#[derive(Clone)]
struct Listener(mpsc::Sender<Event>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

struct Shared {
    osc_title: Option<String>,
    process_name: Option<String>,
    working_directory: Option<PathBuf>,
    exited: bool,
    exit_code: Option<i32>,
    command_line: String,
    command_label: Option<String>,
}

/// A single PTY-backed terminal. Clones share the same process and grid.
pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    events_tx: async_channel::Sender<TermEvent>,
    events: async_channel::Receiver<TermEvent>,
    shared: Arc<Mutex<Shared>>,
    shell_name: String,
    cwd: PathBuf,
    _pty_info: Arc<pty_info::PtyInfo>,
    refresh_running: Arc<AtomicBool>,
}

thread_local! {
    static SPAWN_CWD_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// The interactive shell to spawn, as `(program, args)`.
///
/// - Unix: `$SHELL` (falling back to `/bin/zsh`) as a **login** shell.
/// - Windows: `%COMSPEC%` (falling back to `powershell.exe`). `SHELL` is unset
///   there and `-l` is not a thing — passing either would spawn nothing.
fn default_shell() -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string());
        (shell, Vec::new())
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        (shell, vec!["-l".to_string()])
    }
}

/// The tab label for a shell path: its file stem (`/bin/zsh` → `zsh`,
/// `C:\Windows\system32\cmd.exe` → `cmd`). Both separators are handled
/// explicitly, so a Windows `%COMSPEC%` still labels correctly (and the label
/// stays unit-testable from any host).
fn shell_label(shell: &str) -> String {
    let name = shell
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("shell");
    name.rsplit_once('.')
        .map_or(name, |(stem, _)| stem)
        .to_string()
}

impl Terminal {
    /// Spawn the platform's default interactive shell in `cwd`.
    pub fn spawn(cwd: impl AsRef<Path>) -> io::Result<Self> {
        let (shell, args) = default_shell();
        let shell_name = shell_label(&shell);
        let cwd = SPAWN_CWD_OVERRIDE
            .with(|override_cwd| override_cwd.borrow().clone())
            .unwrap_or_else(|| cwd.as_ref().to_path_buf());
        Self::spawn_command(cwd, shell, args, shell_name)
    }

    fn spawn_command(
        cwd: impl AsRef<Path>,
        program: String,
        args: Vec<String>,
        shell_name: String,
    ) -> io::Result<Self> {
        let cwd = cwd.as_ref().to_path_buf();
        let size = Size {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
        };
        let options = tty::Options {
            shell: Some(tty::Shell::new(program, args)),
            working_directory: Some(cwd.clone()),
            drain_on_exit: true,
            env: HashMap::from([
                ("TERM".to_string(), "xterm-256color".to_string()),
                ("COLORTERM".to_string(), "truecolor".to_string()),
                ("TERM_PROGRAM".to_string(), "tcode".to_string()),
            ]),
            #[cfg(windows)]
            escape_args: false,
        };
        let pty = tty::new(&options, size.window_size(), 0)?;
        #[cfg(unix)]
        let pty_info = {
            use std::os::fd::AsRawFd as _;
            Arc::new(pty_info::PtyInfo::new(
                pty.file().as_raw_fd(),
                pty.child().id(),
            ))
        };
        #[cfg(not(unix))]
        let pty_info = Arc::new(pty_info::PtyInfo::new());
        let (events_tx, events_rx) = mpsc::channel();
        let (term_events_tx, term_events_rx) = async_channel::unbounded();
        let listener = Listener(events_tx);
        let config = Config {
            scrolling_history: 1000,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener.clone())));
        let event_loop = EventLoop::new(term.clone(), listener, pty, true, false)?;
        let notifier = Notifier(event_loop.channel());
        let shared = Arc::new(Mutex::new(Shared {
            osc_title: None,
            process_name: None,
            working_directory: Some(cwd.clone()),
            exited: false,
            exit_code: None,
            command_line: String::new(),
            command_label: None,
        }));
        event_loop.spawn();

        let event_shared = shared.clone();
        // Do not let the event thread keep the terminal (and therefore its
        // Listener sender) alive forever while it blocks on `recv`.
        let event_term = Arc::downgrade(&term);
        let event_notifier = Notifier(notifier.0.clone());
        let event_notifications = term_events_tx.clone();
        let refresh_running = Arc::new(AtomicBool::new(false));
        let event_pty_info = pty_info.clone();
        let refresh_running_events = refresh_running.clone();
        thread::Builder::new()
            .name("tcode-terminal-events".into())
            .spawn(move || {
                while let Ok(event) = events_rx.recv() {
                    match event {
                        Event::Title(title) => {
                            event_shared.lock().unwrap().osc_title = Some(title);
                            let _ = term_events_tx.try_send(TermEvent::TitleChanged);
                        }
                        Event::ResetTitle => {
                            event_shared.lock().unwrap().osc_title = None;
                            let _ = term_events_tx.try_send(TermEvent::TitleChanged);
                        }
                        Event::ChildExit(code) => {
                            let mut shared = event_shared.lock().unwrap();
                            shared.exited = true;
                            shared.exit_code = Some(code);
                            shared.command_label = None;
                            drop(shared);
                            let _ = term_events_tx.try_send(TermEvent::Exited);
                        }
                        Event::Exit => {
                            let mut shared = event_shared.lock().unwrap();
                            if !shared.exited {
                                shared.exited = true;
                                shared.command_label = None;
                                drop(shared);
                                let _ = term_events_tx.try_send(TermEvent::Exited);
                            }
                        }
                        Event::Wakeup => {
                            let _ = term_events_tx.try_send(TermEvent::Wakeup);
                            schedule_process_refresh(
                                event_pty_info.clone(),
                                event_shared.clone(),
                                term_events_tx.clone(),
                                refresh_running_events.clone(),
                            );
                        }
                        Event::CursorBlinkingChange => {
                            let _ = term_events_tx.try_send(TermEvent::CursorBlinkingChanged);
                        }
                        Event::Bell => {
                            let _ = term_events_tx.try_send(TermEvent::Bell);
                        }
                        // The terminal core emits these when the child asks the
                        // emulator a question (DA1/DSR, OSC color queries,
                        // text-area size). Dropping them leaves shells such as
                        // fish blocked until their device-query timeout.
                        Event::PtyWrite(text) => event_notifier.notify(text.into_bytes()),
                        Event::ColorRequest(index, format) => {
                            event_notifier.notify(format(query_color(index)).into_bytes())
                        }
                        Event::TextAreaSizeRequest(format) => {
                            let Some(event_term) = event_term.upgrade() else {
                                continue;
                            };
                            let term = event_term.lock();
                            let size = Size {
                                cols: term.columns(),
                                rows: term.screen_lines(),
                            }
                            .window_size();
                            drop(term);
                            event_notifier.notify(format(size).into_bytes());
                        }
                        _ => {}
                    }
                }
            })?;

        schedule_process_refresh(
            pty_info.clone(),
            shared.clone(),
            event_notifications.clone(),
            refresh_running.clone(),
        );

        Ok(Self {
            term,
            notifier,
            events_tx: event_notifications,
            events: term_events_rx,
            shared,
            shell_name,
            cwd,
            _pty_info: pty_info,
            refresh_running,
        })
    }

    pub fn shell_name(&self) -> &str {
        &self.shell_name
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn working_directory(&self) -> PathBuf {
        self.shared
            .lock()
            .unwrap()
            .working_directory
            .clone()
            .unwrap_or_else(|| self.cwd.clone())
    }

    /// Apply a cwd override to `Terminal::spawn` calls made synchronously by `f`.
    pub fn with_spawn_cwd<R>(cwd: impl Into<PathBuf>, f: impl FnOnce() -> R) -> R {
        struct Reset(Option<PathBuf>);
        impl Drop for Reset {
            fn drop(&mut self) {
                SPAWN_CWD_OVERRIDE.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let previous = SPAWN_CWD_OVERRIDE.with(|slot| slot.borrow_mut().replace(cwd.into()));
        let _reset = Reset(previous);
        f()
    }

    /// Return a receiver for rendering-relevant terminal events.
    ///
    /// Cloned receivers compete for events; callers should create one draining
    /// task per terminal and fan notifications out from there when necessary.
    pub fn events(&self) -> async_channel::Receiver<TermEvent> {
        self.events.clone()
    }

    /// Shell name, temporarily replaced by the argv0 of the last submitted command.
    pub fn label(&self) -> String {
        let shared = self.shared.lock().unwrap();
        shared
            .osc_title
            .clone()
            .or_else(|| shared.process_name.clone())
            .unwrap_or_else(|| self.shell_name.clone())
    }

    pub fn write_input(&self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        let label_changed = {
            let mut shared = self.shared.lock().unwrap();
            let previous_label = shared.command_label.clone();
            track_command_input(&mut shared, &bytes);
            shared.command_label != previous_label
        };
        let _ = self.notifier.0.send(Msg::Input(bytes.into()));
        schedule_process_refresh(
            self._pty_info.clone(),
            self.shared.clone(),
            self.events_tx.clone(),
            self.refresh_running.clone(),
        );
        if label_changed {
            let _ = self.events_tx.try_send(TermEvent::Wakeup);
        }
    }

    pub fn resize(&self, cols: usize, rows: usize) {
        let cols = cols.max(2);
        let rows = rows.max(2);
        {
            let mut term = self.term.lock();
            if term.columns() == cols && term.screen_lines() == rows {
                return;
            }
            term.resize(Size { cols, rows });
        }
        let _ = self
            .notifier
            .0
            .send(Msg::Resize(Size { cols, rows }.window_size()));
        let _ = self.events_tx.try_send(TermEvent::Wakeup);
    }

    pub fn scroll(&self, lines: i32) {
        let mut term = self.term.lock();
        let display_offset = term.grid().display_offset();
        term.scroll_display(Scroll::Delta(lines));
        let changed = term.grid().display_offset() != display_offset;
        drop(term);
        if changed {
            let _ = self.events_tx.try_send(TermEvent::Wakeup);
        }
    }

    /// Start or update a simple selection using zero-based visible grid coordinates.
    pub fn select(&self, start: (usize, usize), end: (usize, usize)) {
        self.select_kind(start, end, SelectionKind::Simple);
    }

    /// Start or update a selection of the requested kind.
    pub fn select_kind(&self, start: (usize, usize), end: (usize, usize), kind: SelectionKind) {
        let mut term = self.term.lock();
        let offset = term.grid().display_offset() as i32;
        let point = |(row, col): (usize, usize)| {
            Point::new(
                Line(row as i32 - offset),
                Column(col.min(term.columns() - 1)),
            )
        };
        let selection_type = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Semantic => SelectionType::Semantic,
            SelectionKind::Lines => SelectionType::Lines,
        };
        let mut selection = Selection::new(selection_type, point(start), Side::Left);
        selection.update(point(end), Side::Right);
        term.selection = Some(selection);
        drop(term);
        let _ = self.events_tx.try_send(TermEvent::Wakeup);
    }

    pub fn extend_selection(&self, end: (usize, usize)) {
        let mut term = self.term.lock();
        let offset = term.grid().display_offset() as i32;
        let point = Point::new(
            Line(end.0 as i32 - offset),
            Column(end.1.min(term.columns() - 1)),
        );
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, Side::Right);
        }
        drop(term);
        let _ = self.events_tx.try_send(TermEvent::Wakeup);
    }

    pub fn clear_selection(&self) {
        let mut term = self.term.lock();
        let changed = term.selection.take().is_some();
        drop(term);
        if changed {
            let _ = self.events_tx.try_send(TermEvent::Wakeup);
        }
    }

    pub fn selected_text(&self) -> Option<SelectedText> {
        let term = self.term.lock();
        let range = term.selection.as_ref()?.to_range(&term)?;
        let history = term.history_size() as i32;
        let line_number = |line: Line| (history + line.0 + 1).max(1) as usize;
        let text = term.selection_to_string()?.trim_matches('\n').to_string();
        if text.is_empty() {
            return None;
        }
        Some(SelectedText {
            line_start: line_number(range.start.line),
            line_end: line_number(range.end.line),
            text,
        })
    }

    pub fn snapshot(&self) -> TermState {
        let term = self.term.lock();
        let content = term.renderable_content();
        let cursor_style = term.cursor_style();
        let cols = term.columns();
        let rows = term.screen_lines();
        let display_cursor_line = content.cursor.point.line.0 + content.display_offset as i32;
        let cursor = if display_cursor_line >= 0 && display_cursor_line < rows as i32 {
            Some((display_cursor_line as usize, content.cursor.point.column.0))
        } else {
            None
        };
        let selection = content.selection;
        let cells = content
            .display_iter
            .map(|indexed| {
                let selected = selection.is_some_and(|range| {
                    range.contains_cell(&indexed, content.cursor.point, content.cursor.shape)
                });
                let cell = indexed.cell;
                let mut text = String::from(cell.c);
                text.extend(cell.zerowidth().into_iter().flatten());
                Cell {
                    ch: cell.c,
                    text,
                    fg: convert_color(cell.fg),
                    bg: convert_color(cell.bg),
                    bold: cell.flags.contains(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                    inverse: cell.flags.contains(Flags::INVERSE),
                    selected,
                    wide: cell.flags.contains(Flags::WIDE_CHAR),
                    wide_spacer: cell.flags.contains(Flags::WIDE_CHAR_SPACER),
                }
            })
            .collect();
        let shared = self.shared.lock().unwrap();
        let mode = *term.mode();
        TermState {
            cols,
            rows,
            cells,
            cursor,
            cursor_shape: map_cursor_shape(content.cursor.shape),
            cursor_blinking: cursor_style.blinking,
            title: shared
                .osc_title
                .clone()
                .or_else(|| shared.process_name.clone())
                .unwrap_or_else(|| self.shell_name.clone()),
            exited: shared.exited,
            exit_code: shared.exit_code,
            display_offset: content.display_offset,
            history_size: term.history_size(),
            mode: ModeSnapshot {
                mouse_click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
                mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
                mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
                sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
                utf8_mouse: mode.contains(TermMode::UTF8_MOUSE),
                alt_screen: mode.contains(TermMode::ALT_SCREEN),
                alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
                bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
                app_cursor: mode.contains(TermMode::APP_CURSOR),
                focus_in_out: mode.contains(TermMode::FOCUS_IN_OUT),
            },
        }
    }

    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<HyperlinkMatch> {
        let term = self.term.lock();
        (row < term.screen_lines() && col < term.columns())
            .then(|| hyperlinks::find(&term, row, col))
            .flatten()
    }
}

fn refresh_process_info(
    info: &pty_info::PtyInfo,
    shared: &Mutex<Shared>,
    notifications: &async_channel::Sender<TermEvent>,
) {
    if let Some(process) = info.load() {
        let mut shared = shared.lock().unwrap();
        let changed = shared.process_name.as_deref() != Some(&process.name)
            || shared.working_directory.as_ref() != Some(&process.cwd);
        shared.process_name = Some(process.name);
        shared.working_directory = Some(process.cwd);
        drop(shared);
        if changed {
            let _ = notifications.try_send(TermEvent::TitleChanged);
        }
    }
}

fn schedule_process_refresh(
    info: Arc<pty_info::PtyInfo>,
    shared: Arc<Mutex<Shared>>,
    notifications: async_channel::Sender<TermEvent>,
    running: Arc<AtomicBool>,
) {
    if !info.should_refresh() || running.swap(true, Ordering::AcqRel) {
        return;
    }
    let running_on_error = running.clone();
    let spawn_result = thread::Builder::new()
        .name("tcode-terminal-pty-info".into())
        .spawn(move || {
            refresh_process_info(&info, &shared, &notifications);
            // Input/Wakeup can precede the shell applying `cd` or launching its
            // foreground process. One bounded trailing refresh catches that
            // transition even when the command itself produces no output.
            thread::sleep(Duration::from_millis(300));
            refresh_process_info(&info, &shared, &notifications);
            running.store(false, Ordering::Release);
        });
    if spawn_result.is_err() {
        running_on_error.store(false, Ordering::Release);
    }
}

fn map_cursor_shape(shape: alacritty_terminal::vte::ansi::CursorShape) -> CursorShape {
    use alacritty_terminal::vte::ansi::CursorShape as A;
    match shape {
        A::Block => CursorShape::Block,
        A::Underline => CursorShape::Underline,
        A::Beam => CursorShape::Bar,
        A::HollowBlock => CursorShape::HollowBlock,
        A::Hidden => CursorShape::Hidden,
    }
}

fn query_color(index: usize) -> Rgb {
    const ANSI: [u32; 16] = [
        0x1f2329, 0xe45649, 0x50a14f, 0xc18401, 0x4078f2, 0xa626a4, 0x0184bc, 0xabb2bf, 0x5c6370,
        0xff616e, 0x7bc275, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
    ];
    let value = match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let n = (index - 16) as u32;
            let component = |value: u32| if value == 0 { 0 } else { 55 + value * 40 };
            (component(n / 36) << 16) | (component((n % 36) / 6) << 8) | component(n % 6)
        }
        232..=255 => {
            let gray = 8 + (index - 232) as u32 * 10;
            (gray << 16) | (gray << 8) | gray
        }
        index if index == NamedColor::Background as usize => 0xffffff,
        index if index == NamedColor::Cursor as usize => 0x1f2329,
        _ => 0x1f2329,
    };
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

/// Derive the compact tab label used after a command is submitted.
pub fn derive_command_label(command: &str) -> Option<String> {
    let first = command.split_whitespace().next()?;
    let first = first.rsplit('/').next().unwrap_or(first);
    (!first.is_empty()).then(|| first.to_string())
}

fn track_command_input(shared: &mut Shared, bytes: &[u8]) {
    for &byte in bytes {
        match byte {
            3 => {
                shared.command_line.clear();
                shared.command_label = None;
            }
            b'\r' | b'\n' => {
                shared.command_label = derive_command_label(&shared.command_line);
                shared.command_line.clear();
            }
            0x7f | 0x08 => {
                shared.command_line.pop();
            }
            0x20..=0x7e => shared.command_line.push(byte as char),
            _ => {}
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

fn convert_color(color: AlacrittyColor) -> Color {
    match color {
        AlacrittyColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AlacrittyColor::Indexed(index) => Color::Indexed(index),
        AlacrittyColor::Named(named) => match named {
            NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground => {
                Color::DefaultForeground
            }
            NamedColor::Background => Color::DefaultBackground,
            NamedColor::Cursor => Color::DefaultForeground,
            NamedColor::DimBlack => Color::Indexed(0),
            NamedColor::DimRed => Color::Indexed(1),
            NamedColor::DimGreen => Color::Indexed(2),
            NamedColor::DimYellow => Color::Indexed(3),
            NamedColor::DimBlue => Color::Indexed(4),
            NamedColor::DimMagenta => Color::Indexed(5),
            NamedColor::DimCyan => Color::Indexed(6),
            NamedColor::DimWhite => Color::Indexed(7),
            named => Color::Indexed(named as u8),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The shell defaults are platform-specific: `$SHELL -l` on Unix, `%COMSPEC%`
    /// (no `-l`; the flag does not exist there) on Windows.
    #[test]
    fn default_shell_matches_the_platform() {
        let (program, args) = default_shell();
        if cfg!(windows) {
            assert!(
                args.is_empty(),
                "Windows shells take no login flag, got {args:?}"
            );
            assert!(
                program.to_lowercase().contains("cmd")
                    || program.to_lowercase().contains("powershell"),
                "unexpected Windows shell {program}"
            );
        } else {
            assert_eq!(args, vec!["-l".to_string()]);
            assert!(program.starts_with('/'), "unexpected Unix shell {program}");
        }
    }

    #[test]
    fn shell_label_is_the_file_stem() {
        assert_eq!(shell_label("/bin/zsh"), "zsh");
        assert_eq!(shell_label(r"C:\Windows\system32\cmd.exe"), "cmd");
    }

    #[test]
    fn maps_all_alacritty_cursor_shapes() {
        use alacritty_terminal::vte::ansi::CursorShape as A;
        assert_eq!(map_cursor_shape(A::Block), CursorShape::Block);
        assert_eq!(map_cursor_shape(A::Underline), CursorShape::Underline);
        assert_eq!(map_cursor_shape(A::Beam), CursorShape::Bar);
        assert_eq!(map_cursor_shape(A::HollowBlock), CursorShape::HollowBlock);
        assert_eq!(map_cursor_shape(A::Hidden), CursorShape::Hidden);
    }

    /// Real-PTY tests need a live shell. CI sets TCODE_LIVE_TESTS=0 because its
    /// shared runners intermittently fail to service PTYs at all (observed
    /// 2026-07-13: /bin/sh under a PTY produced no output for 120 seconds).
    fn live_pty_denied() -> bool {
        std::env::var("TCODE_LIVE_TESTS").is_ok_and(|v| v == "0")
    }

    macro_rules! require_live_pty {
        () => {
            if live_pty_denied() {
                eprintln!("skipped: TCODE_LIVE_TESTS=0");
                return;
            }
        };
    }

    /// A shell that runs `script` and exits: `sh -c` on Unix, `cmd /c` on Windows.
    fn command(script: &str) -> Terminal {
        #[cfg(windows)]
        let (program, args, name) = (
            "cmd.exe".to_string(),
            vec!["/c".to_string(), script.to_string()],
            "cmd".to_string(),
        );
        #[cfg(not(windows))]
        let (program, args, name) = (
            "/bin/sh".to_string(),
            vec!["-c".to_string(), script.to_string()],
            "sh".to_string(),
        );
        Terminal::spawn_command(std::env::temp_dir(), program, args, name).unwrap()
    }

    fn wait_until(term: &Terminal, predicate: impl Fn(&TermState) -> bool) -> TermState {
        let start = Instant::now();
        loop {
            let state = term.snapshot();
            if predicate(&state) {
                return state;
            }
            // Generous: this waits on a real PTY under a shared CI runner, where
            // spawning a shell can stall for tens of seconds on a degraded host
            // (observed 2026-07-13). The test is about the output eventually
            // arriving, not about how fast.
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "terminal timed out: {:?}",
                state.text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The PTY tests below drive real shell syntax, so they are split per
    /// platform: POSIX `sh` scripts on Unix, `cmd` on Windows (ConPTY).
    #[cfg(unix)]
    #[test]
    fn captures_process_output_and_exit() {
        require_live_pty!();
        let term = command("printf 'hello\\n'");
        let state = wait_until(&term, |state| {
            state.text().contains("hello") && state.exited
        });
        assert_eq!(state.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_mouse_mode_changes_drawer_routing_decision() {
        require_live_pty!();
        let term = command("printf '\\033[?1002h\\033[?1006h'; sleep 1");
        let state = wait_until(&term, |state| state.mode.mouse_drag && state.mode.sgr_mouse);
        assert!(state.mode.routes_mouse(false));
        assert!(!state.mode.routes_mouse(true));
    }

    #[cfg(unix)]
    #[test]
    fn extracts_url_from_grid_line() {
        require_live_pty!();
        let term = command("printf 'see https://example.com/docs?q=1 now\\n'; sleep 1");
        let state = wait_until(&term, |state| {
            state.text().contains("https://example.com/docs?q=1")
        });
        let index = state.cells.iter().position(|cell| cell.ch == 'h').unwrap();
        let found = term
            .hyperlink_at(index / state.cols, index % state.cols + 10)
            .unwrap();
        assert_eq!(found.url, "https://example.com/docs?q=1");
    }

    #[cfg(unix)]
    #[test]
    fn osc8_hyperlink_takes_precedence_over_visible_text() {
        require_live_pty!();
        let term = command(
            "printf '\\033]8;;https://example.com/target\\033\\\\click-me\\033]8;;\\033\\\\'; sleep 1",
        );
        let state = wait_until(&term, |state| state.text().contains("click-me"));
        let index = state.cells.iter().position(|cell| cell.ch == 'c').unwrap();
        let found = term
            .hyperlink_at(index / state.cols, index % state.cols)
            .unwrap();
        assert_eq!(found.url, "https://example.com/target");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "queries a real foreground PTY process group"]
    fn tracks_real_pty_foreground_cwd() {
        let term = command("cd /tmp && sleep 5");
        let expected = std::fs::canonicalize("/tmp").unwrap();
        let start = Instant::now();
        while term.working_directory() != expected {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "cwd remained {}",
                term.working_directory().display()
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(term.working_directory(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn emits_wakeup_when_pty_output_arrives() {
        require_live_pty!();
        let term = command("printf '__TCODE_TERM_READY__\\n'; read line; printf '%s\\n' \"$line\"");
        let events = term.events();

        wait_until(&term, |state| state.text().contains("__TCODE_TERM_READY__"));
        // Wait for the forwarding thread to go quiet so a late readiness event
        // cannot satisfy the assertion for the next write.
        let settle_started = Instant::now();
        let mut quiet_since = Instant::now();
        let mut saw_readiness_event = false;
        while !saw_readiness_event || quiet_since.elapsed() < Duration::from_millis(50) {
            match events.try_recv() {
                Ok(TermEvent::Wakeup) => {
                    saw_readiness_event = true;
                    quiet_since = Instant::now();
                }
                Ok(
                    TermEvent::TitleChanged
                    | TermEvent::CursorBlinkingChanged
                    | TermEvent::Bell
                    | TermEvent::Exited,
                ) => {
                    quiet_since = Instant::now();
                }
                Err(async_channel::TryRecvError::Empty) => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("terminal event stream closed while settling startup events")
                }
            }
            assert!(
                settle_started.elapsed() < Duration::from_secs(5),
                "terminal event stream did not settle after readiness output"
            );
        }

        // Bypass write_input's own label-change Wakeup: this test isolates the
        // Alacritty Wakeup generated when the PTY produces output.
        let _ = term
            .notifier
            .0
            .send(Msg::Input(b"__TCODE_TERM_WAKEUP__\r".to_vec().into()));

        let started = Instant::now();
        let mut saw_wakeup = false;
        loop {
            match events.try_recv() {
                Ok(TermEvent::Wakeup) => saw_wakeup = true,
                Ok(
                    TermEvent::TitleChanged
                    | TermEvent::CursorBlinkingChanged
                    | TermEvent::Bell
                    | TermEvent::Exited,
                ) => {}
                Err(async_channel::TryRecvError::Empty) => {}
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("terminal event stream closed before emitting Wakeup")
                }
            }

            if saw_wakeup && term.snapshot().text().contains("__TCODE_TERM_WAKEUP__") {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "terminal event stream timed out waiting for PTY output"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_wide_cells_spacers_and_combining_characters() {
        require_live_pty!();
        let term = command("echo '中文e\u{301}'; sleep 1");
        let state = wait_until(&term, |state| state.text().contains("中文e\u{301}"));

        let first = state
            .cells
            .iter()
            .position(|cell| cell.ch == '中')
            .expect("snapshot should contain the first CJK cell");
        let row = first / state.cols;
        let column = first % state.cols;

        assert_eq!(state.cells[first].text, "中");
        assert!(state.cells[first].wide);
        assert!(state.cells[first + 1].wide_spacer);
        assert_eq!(state.cells[first + 2].text, "文");
        assert!(state.cells[first + 2].wide);
        assert!(state.cells[first + 3].wide_spacer);
        assert_eq!(state.cells[first + 4].text, "e\u{301}");
        assert!(state.text().contains("中文e\u{301}"));
        assert!(!state.text().contains("中 文"));

        term.select_kind((row, column), (row, column + 3), SelectionKind::Simple);
        assert_eq!(term.selected_text().unwrap().text, "中文");

        // Starting the drag on the trailing half still selects/highlights the
        // complete wide glyph; copied text never exposes spacer cells.
        term.select_kind((row, column + 1), (row, column + 3), SelectionKind::Simple);
        assert_eq!(term.selected_text().unwrap().text, "中文");
        let selected = term.snapshot();
        assert!(selected.cells[first].selected);
        assert!(selected.cells[first + 1].selected);
    }

    #[cfg(unix)]
    #[test]
    fn forwards_primary_device_attribute_response_to_pty() {
        require_live_pty!();
        let term = command(
            "saved=$(stty -g); stty raw -echo; printf '\\033[c'; response=$(dd bs=1 count=5 2>/dev/null); stty \"$saved\"; printf '%s' \"$response\" | od -An -tx1; printf '\\n'",
        );
        let state = wait_until(&term, |state| {
            let text = state.text();
            let fields = text.split_whitespace().collect::<Vec<_>>();
            state.exited
                && fields
                    .windows(5)
                    .any(|window| window == ["1b", "5b", "3f", "36", "63"])
        });
        assert_eq!(state.exit_code, Some(0));
    }

    #[cfg(windows)]
    #[test]
    fn captures_process_output_and_exit() {
        require_live_pty!();
        let term = command("echo hello");
        let state = wait_until(&term, |state| {
            state.text().contains("hello") && state.exited
        });
        assert_eq!(state.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn resizes_grid() {
        require_live_pty!();
        let term = command("sleep 1");
        term.resize(42, 9);
        let state = term.snapshot();
        assert_eq!((state.cols, state.rows), (42, 9));
    }

    #[cfg(windows)]
    #[test]
    fn resizes_grid() {
        require_live_pty!();
        let term = command("timeout /t 1 >nul");
        term.resize(42, 9);
        let state = term.snapshot();
        assert_eq!((state.cols, state.rows), (42, 9));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_input() {
        require_live_pty!();
        let term = command("read line; printf '%s\\n' \"$line\"");
        term.write_input(b"echo tcode-term-ok\r".to_vec());
        let state = wait_until(&term, |state| state.text().contains("echo tcode-term-ok"));
        assert!(state.text().contains("echo tcode-term-ok"));
    }

    #[cfg(unix)]
    #[test]
    fn handles_large_output_and_scrollback() {
        require_live_pty!();
        let term = command("seq 1 5000");
        let state = wait_until(&term, |state| state.exited && state.text().contains("5000"));
        assert_eq!(state.exit_code, Some(0));

        term.scroll(800);
        let scrolled = term.snapshot();
        assert!(scrolled.display_offset > 0);
        assert!(!scrolled.text().contains("5000"));
    }

    /// Manual launch-environment smoke test. It is ignored in the normal suite
    /// because it deliberately loads the developer's real login-shell config.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn default_login_shell_accepts_input_and_history() {
        let started = Instant::now();
        let term = Terminal::spawn(std::env::temp_dir()).unwrap();
        term.write_input(b"echo __TCODE_SHELL_READY__\r".to_vec());
        let ready = wait_until(&term, |state| {
            state.text().contains("__TCODE_SHELL_READY__")
        });
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "login shell did not become interactive within two seconds"
        );
        assert!(!ready.text().contains("could not read response"));

        term.write_input(b"\x1b[A\r".to_vec());
        let history = wait_until(&term, |state| {
            state.text().matches("__TCODE_SHELL_READY__").count() >= 4
        });
        assert!(!history.text().contains("could not read response"));
    }

    /// Manual PTY/TUI smoke test for the macOS `top` used in the bug pass.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn top_starts_and_quits_with_q() {
        let term = Terminal::spawn_command(
            std::env::temp_dir(),
            "/usr/bin/top".to_string(),
            Vec::new(),
            "top".to_string(),
        )
        .unwrap();
        wait_until(&term, |state| {
            state.text().contains("Processes:") || state.text().contains("PID")
        });
        term.write_input(b"q".to_vec());
        let state = wait_until(&term, |state| state.exited);
        assert_eq!(state.exit_code, Some(0));
    }

    #[cfg(windows)]
    #[test]
    fn accepts_input() {
        require_live_pty!();
        let term = command("set /p line= && echo %line%");
        term.write_input(b"tcode-term-ok\r".to_vec());
        let state = wait_until(&term, |state| state.text().contains("tcode-term-ok"));
        assert!(state.text().contains("tcode-term-ok"));
    }

    #[test]
    fn derives_command_label_from_argv0() {
        assert_eq!(
            derive_command_label("  /usr/bin/cargo test --workspace"),
            Some("cargo".into())
        );
        assert_eq!(derive_command_label("   "), None);
    }

    #[cfg(unix)]
    #[test]
    fn programmatic_selection_returns_grid_text() {
        require_live_pty!();
        let term = command("printf 'alpha\\nbeta\\n'; sleep 1");
        let state = wait_until(&term, |state| state.text().contains("beta"));
        let alpha_row = state
            .text()
            .lines()
            .position(|line| line.contains("alpha"))
            .unwrap();
        let beta_row = state
            .text()
            .lines()
            .position(|line| line.contains("beta"))
            .unwrap();
        term.select_kind((alpha_row, 0), (beta_row, 3), SelectionKind::Simple);
        let selected = term.selected_text().unwrap();
        assert_eq!(selected.text, "alpha\nbeta");
        assert_eq!(selected.line_end, selected.line_start + 1);
        assert!(term.snapshot().cells.iter().any(|cell| cell.selected));
    }
}
