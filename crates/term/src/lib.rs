//! Small, UI-agnostic terminal runtime built on Alacritty's terminal core.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener, Notify as _, WindowSize},
    event_loop::{EventLoop, Msg, Notifier},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{Config, cell::Flags},
    tty,
    vte::ansi::{Color as AlacrittyColor, NamedColor, Rgb},
};

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
    pub title: String,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub display_offset: usize,
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
    title: String,
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
            title: shell_name.clone(),
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
        let default_title = shell_name.clone();
        thread::Builder::new()
            .name("tcode-terminal-events".into())
            .spawn(move || {
                while let Ok(event) = events_rx.recv() {
                    match event {
                        Event::Title(title) => {
                            event_shared.lock().unwrap().title = title;
                            let _ = term_events_tx.try_send(TermEvent::TitleChanged);
                        }
                        Event::ResetTitle => {
                            event_shared
                                .lock()
                                .unwrap()
                                .title
                                .clone_from(&default_title);
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
                        Event::Wakeup | Event::CursorBlinkingChange => {
                            let _ = term_events_tx.try_send(TermEvent::Wakeup);
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

        Ok(Self {
            term,
            notifier,
            events_tx: event_notifications,
            events: term_events_rx,
            shared,
            shell_name,
            cwd,
        })
    }

    pub fn shell_name(&self) -> &str {
        &self.shell_name
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
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
        self.shared
            .lock()
            .unwrap()
            .command_label
            .clone()
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
        let mut term = self.term.lock();
        let offset = term.grid().display_offset() as i32;
        let point = |(row, col): (usize, usize)| {
            Point::new(
                Line(row as i32 - offset),
                Column(col.min(term.columns() - 1)),
            )
        };
        let mut selection = Selection::new(SelectionType::Simple, point(start), Side::Left);
        selection.update(point(end), Side::Right);
        term.selection = Some(selection);
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
        TermState {
            cols,
            rows,
            cells,
            cursor,
            title: shared.title.clone(),
            exited: shared.exited,
            exit_code: shared.exit_code,
            display_offset: content.display_offset,
        }
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
            // spawning a shell can stall for seconds. The test is about the
            // output eventually arriving, not about how fast.
            assert!(
                start.elapsed() < Duration::from_secs(30),
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
        let term = command("printf 'hello\\n'");
        let state = wait_until(&term, |state| {
            state.text().contains("hello") && state.exited
        });
        assert_eq!(state.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn emits_wakeup_when_pty_output_arrives() {
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
                Ok(TermEvent::TitleChanged | TermEvent::Exited) => {
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
                Ok(TermEvent::TitleChanged | TermEvent::Exited) => {}
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

        term.select((row, column), (row, column + 3));
        assert_eq!(term.selected_text().unwrap().text, "中文");

        // Starting the drag on the trailing half still selects/highlights the
        // complete wide glyph; copied text never exposes spacer cells.
        term.select((row, column + 1), (row, column + 3));
        assert_eq!(term.selected_text().unwrap().text, "中文");
        let selected = term.snapshot();
        assert!(selected.cells[first].selected);
        assert!(selected.cells[first + 1].selected);
    }

    #[cfg(unix)]
    #[test]
    fn forwards_primary_device_attribute_response_to_pty() {
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
        let term = command("echo hello");
        let state = wait_until(&term, |state| {
            state.text().contains("hello") && state.exited
        });
        assert_eq!(state.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn resizes_grid() {
        let term = command("sleep 1");
        term.resize(42, 9);
        let state = term.snapshot();
        assert_eq!((state.cols, state.rows), (42, 9));
    }

    #[cfg(windows)]
    #[test]
    fn resizes_grid() {
        let term = command("timeout /t 1 >nul");
        term.resize(42, 9);
        let state = term.snapshot();
        assert_eq!((state.cols, state.rows), (42, 9));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_input() {
        let term = command("read line; printf '%s\\n' \"$line\"");
        term.write_input(b"echo tcode-term-ok\r".to_vec());
        let state = wait_until(&term, |state| state.text().contains("echo tcode-term-ok"));
        assert!(state.text().contains("echo tcode-term-ok"));
    }

    #[cfg(unix)]
    #[test]
    fn handles_large_output_and_scrollback() {
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
        term.select((alpha_row, 0), (beta_row, 3));
        let selected = term.selected_text().unwrap();
        assert_eq!(selected.text, "alpha\nbeta");
        assert_eq!(selected.line_end, selected.line_start + 1);
        assert!(term.snapshot().cells.iter().any(|cell| cell.selected));
    }
}
