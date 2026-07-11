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
    event::{Event, EventListener, WindowSize},
    event_loop::{EventLoop, Msg, Notifier},
    grid::{Dimensions, Scroll},
    sync::FairMutex,
    term::{Config, cell::Flags},
    tty,
    vte::ansi::{Color as AlacrittyColor, NamedColor},
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
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
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
                row.iter()
                    .map(|cell| cell.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
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
}

/// A single PTY-backed terminal. Clones share the same process and grid.
pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    shared: Arc<Mutex<Shared>>,
    shell_name: String,
    cwd: PathBuf,
}

impl Terminal {
    /// Spawn `$SHELL` as a login shell in `cwd`.
    pub fn spawn(cwd: impl AsRef<Path>) -> io::Result<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let shell_name = Path::new(&shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("shell")
            .to_string();
        Self::spawn_command(cwd, shell, vec!["-l".to_string()], shell_name)
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
        }));
        event_loop.spawn();

        let event_shared = shared.clone();
        thread::Builder::new()
            .name("tcode-terminal-events".into())
            .spawn(move || {
                while let Ok(event) = events_rx.recv() {
                    let mut shared = event_shared.lock().unwrap();
                    match event {
                        Event::Title(title) => shared.title = title,
                        Event::ChildExit(code) => {
                            shared.exited = true;
                            shared.exit_code = Some(code);
                        }
                        _ => {}
                    }
                }
            })?;

        Ok(Self {
            term,
            notifier,
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

    pub fn write_input(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.notifier.0.send(Msg::Input(bytes.into().into()));
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
    }

    pub fn scroll(&self, lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(lines));
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
        let cells = content
            .display_iter
            .map(|indexed| {
                let cell = indexed.cell;
                Cell {
                    ch: cell.c,
                    fg: convert_color(cell.fg),
                    bg: convert_color(cell.bg),
                    bold: cell.flags.contains(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                    inverse: cell.flags.contains(Flags::INVERSE),
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

    fn command(script: &str) -> Terminal {
        Terminal::spawn_command(
            std::env::temp_dir(),
            "/bin/sh".into(),
            vec!["-c".into(), script.into()],
            "sh".into(),
        )
        .unwrap()
    }

    fn wait_until(term: &Terminal, predicate: impl Fn(&TermState) -> bool) -> TermState {
        let start = Instant::now();
        loop {
            let state = term.snapshot();
            if predicate(&state) {
                return state;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "terminal timed out: {:?}",
                state.text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn captures_process_output_and_exit() {
        let term = command("printf 'hello\\n'");
        let state = wait_until(&term, |state| {
            state.text().contains("hello") && state.exited
        });
        assert_eq!(state.exit_code, Some(0));
    }

    #[test]
    fn resizes_grid() {
        let term = command("sleep 1");
        term.resize(42, 9);
        let state = term.snapshot();
        assert_eq!((state.cols, state.rows), (42, 9));
    }

    #[test]
    fn accepts_input() {
        let term = command("read line; printf '%s\\n' \"$line\"");
        term.write_input(b"echo tcode-term-ok\r".to_vec());
        let state = wait_until(&term, |state| state.text().contains("echo tcode-term-ok"));
        assert!(state.text().contains("echo tcode-term-ok"));
    }
}
