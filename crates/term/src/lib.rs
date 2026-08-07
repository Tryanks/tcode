//! UI-agnostic terminal process management and emulation.

use std::{
    io,
    path::{Path, PathBuf},
    thread,
};

mod grid_emulator;
mod hyperlinks;
pub mod mappings;
mod pty;
mod pty_info;

pub use hyperlinks::HyperlinkMatch;

use grid_emulator::{GridEmulator, GridEvent};
use pty::{PtyEvent, PtyHandle};

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

/// A rendering-relevant event emitted by the compatibility terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermEvent {
    /// The terminal grid or cursor changed.
    Wakeup,
    /// The terminal title or foreground process changed.
    TitleChanged,
    /// The child changed whether its cursor should blink.
    CursorBlinkingChanged,
    /// The emulated terminal rang the bell.
    Bell,
    /// The host-side child process exited.
    Exited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedText {
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionSide {
    Left,
    Right,
}

pub struct Terminal {
    pty: PtyHandle,
    emulator: GridEmulator,
    notifications: async_channel::Sender<TermEvent>,
    events: async_channel::Receiver<TermEvent>,
}

impl Terminal {
    /// Resolve the cwd that a subsequent [`Terminal::spawn`] should use.
    pub fn resolve_spawn_cwd(cwd: impl AsRef<Path>) -> PathBuf {
        PtyHandle::resolve_spawn_cwd(cwd)
    }

    /// Spawn the platform's default interactive shell in `cwd`.
    pub fn spawn(cwd: impl AsRef<Path>) -> io::Result<Self> {
        Self::from_pty(PtyHandle::spawn(cwd)?)
    }

    #[cfg(test)]
    fn spawn_command(
        cwd: impl AsRef<Path>,
        program: String,
        args: Vec<String>,
        shell_name: String,
    ) -> io::Result<Self> {
        Self::from_pty(PtyHandle::spawn_command(cwd, program, args, shell_name)?)
    }

    fn from_pty(pty: PtyHandle) -> io::Result<Self> {
        let emulator = GridEmulator::with_size_and_title(DEFAULT_COLS, DEFAULT_ROWS, pty.label());
        Self::from_parts(pty, emulator)
    }

    fn from_parts(pty: PtyHandle, emulator: GridEmulator) -> io::Result<Self> {
        emulator.set_fallback_title(pty.label());
        if pty.exited() {
            emulator.set_exited(pty.exit_code());
        }

        let (notifications, events) = async_channel::unbounded();
        let pty_events = pty.events();
        let output_emulator = emulator.clone();
        let pty_notifications = notifications.clone();
        thread::Builder::new()
            .name("tcode-terminal-output-bridge".into())
            .spawn(move || {
                while let Ok(event) = pty_events.recv_blocking() {
                    match event {
                        PtyEvent::Output(bytes) => output_emulator.feed(&bytes),
                        PtyEvent::ProcessInfoChanged { name, .. } => {
                            output_emulator.set_fallback_title(name);
                            let _ = pty_notifications.try_send(TermEvent::TitleChanged);
                        }
                        PtyEvent::Exited { exit_code } => {
                            output_emulator.set_exited(exit_code);
                            let _ = pty_notifications.try_send(TermEvent::Exited);
                            let _ = pty_notifications.try_send(TermEvent::Wakeup);
                        }
                    }
                }
            })?;

        let grid_events = emulator.events();
        let writer = pty.writer();
        let grid_notifications = notifications.clone();
        thread::Builder::new()
            .name("tcode-terminal-grid-events".into())
            .spawn(move || {
                while let Ok(event) = grid_events.recv_blocking() {
                    let notification = match event {
                        GridEvent::Wakeup => Some(TermEvent::Wakeup),
                        GridEvent::TitleChanged(_) => Some(TermEvent::TitleChanged),
                        GridEvent::CursorBlinkingChanged => Some(TermEvent::CursorBlinkingChanged),
                        GridEvent::Bell => Some(TermEvent::Bell),
                        GridEvent::Input(bytes) => {
                            writer.write_raw(bytes);
                            None
                        }
                    };
                    if let Some(notification) = notification {
                        let _ = grid_notifications.try_send(notification);
                    }
                }
            })?;

        Ok(Self {
            pty,
            emulator,
            notifications,
            events,
        })
    }

    pub fn shell_name(&self) -> &str {
        self.pty.shell_name()
    }

    pub fn cwd(&self) -> &Path {
        self.pty.cwd()
    }

    pub fn working_directory(&self) -> PathBuf {
        self.pty.working_directory()
    }

    /// Apply a cwd override to [`Terminal::spawn`] calls made synchronously by `f`.
    pub fn with_spawn_cwd<R>(cwd: impl Into<PathBuf>, f: impl FnOnce() -> R) -> R {
        PtyHandle::with_spawn_cwd(cwd, f)
    }

    /// Return a receiver for rendering-relevant compatibility events.
    ///
    /// Cloned receivers compete for events; callers should create one draining
    /// task per terminal and fan notifications out from there when necessary.
    pub fn events(&self) -> async_channel::Receiver<TermEvent> {
        self.events.clone()
    }

    pub fn label(&self) -> String {
        self.emulator
            .osc_title()
            .unwrap_or_else(|| self.pty.label())
    }

    pub fn write_input(&self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        self.emulator.prepare_input();
        if self.pty.write_input_inner(bytes).unwrap_or(false) {
            let _ = self.notifications.try_send(TermEvent::Wakeup);
        }
    }

    /// Send terminal protocol bytes to the PTY without changing viewport or selection state.
    pub fn write_raw(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.pty.write_raw(bytes);
    }

    pub fn resize(&self, cols: usize, rows: usize) {
        if self.emulator.resize_if_changed(cols, rows) {
            let _ = self.pty.resize(cols, rows);
        }
    }

    pub fn kill(&self) {
        let _ = self.pty.kill();
    }

    pub fn scroll(&self, lines: i32) {
        self.emulator.scroll(lines);
    }

    pub fn select(&self, start: (usize, usize), end: (usize, usize)) {
        self.emulator.select(start, end);
    }

    pub fn start_selection(&self, kind: SelectionKind, point: (usize, usize), side: SelectionSide) {
        self.emulator.start_selection(kind, point, side);
    }

    pub fn update_selection(&self, point: (usize, usize), side: SelectionSide) {
        self.emulator.update_selection(point, side);
    }

    pub fn clear_selection(&self) {
        self.emulator.clear_selection();
    }

    pub fn select_all(&self) {
        self.emulator.select_all();
    }

    pub fn clear(&self) {
        self.emulator.clear();
    }

    pub fn selected_text(&self) -> Option<SelectedText> {
        self.emulator.selected_text()
    }

    pub fn snapshot(&self) -> TermState {
        let mut snapshot = self.emulator.snapshot();
        snapshot.title = self.label();
        snapshot.exited = self.pty.exited();
        snapshot.exit_code = self.pty.exit_code();
        snapshot
    }

    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<HyperlinkMatch> {
        self.emulator.hyperlink_at(row, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[cfg(not(windows))]
    use crate::pty::unix_shell;
    use crate::pty::{default_shell, shell_label};

    fn live_pty_denied() -> bool {
        std::env::var("TCODE_LIVE_TESTS").is_ok_and(|value| value == "0")
    }

    macro_rules! require_live_pty {
        () => {
            if live_pty_denied() {
                eprintln!("skipped: TCODE_LIVE_TESTS=0");
                return;
            }
        };
    }

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

    fn wait_until(terminal: &Terminal, predicate: impl Fn(&TermState) -> bool) -> TermState {
        let start = Instant::now();
        loop {
            let state = terminal.snapshot();
            if predicate(&state) {
                return state;
            }
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "terminal timed out: {:?}",
                state.text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn default_shell_matches_the_platform() {
        let (program, args) = default_shell();
        if cfg!(windows) {
            assert!(args.is_empty());
            assert!(
                program.to_lowercase().contains("cmd")
                    || program.to_lowercase().contains("powershell")
            );
        } else {
            assert_eq!(args, vec!["-l".to_string()]);
            assert!(program.starts_with('/'));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_shell_uses_explicit_shell() {
        assert_eq!(unix_shell(Some("/usr/bin/fish")), "/usr/bin/fish");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_shell_falls_back_to_bin_sh_when_unset_or_empty() {
        assert_eq!(unix_shell(None), "/bin/sh");
        assert_eq!(unix_shell(Some("")), "/bin/sh");
        assert_eq!(unix_shell(Some("  \t")), "/bin/sh");
    }

    #[test]
    fn shell_label_is_the_file_stem() {
        assert_eq!(shell_label("/bin/zsh"), "zsh");
        assert_eq!(shell_label(r"C:\Windows\system32\cmd.exe"), "cmd");
    }

    #[test]
    fn local_transport_preserves_terminal_output_and_exit_without_json() {
        let (sender, receiver) = async_channel::unbounded();
        let output = vec![0, b'\n', 255];
        sender
            .try_send(PtyEvent::Output(output.clone()))
            .expect("send raw output");
        sender
            .try_send(PtyEvent::Exited { exit_code: Some(0) })
            .expect("send exit status");

        assert_eq!(
            receiver.try_recv().expect("receive raw output"),
            PtyEvent::Output(output)
        );
        assert_eq!(
            receiver.try_recv().expect("receive exit status"),
            PtyEvent::Exited { exit_code: Some(0) }
        );
    }

    #[cfg(unix)]
    #[test]
    fn captures_process_output_and_exit() {
        require_live_pty!();
        let terminal = command("printf 'hello\\n'");
        let state = wait_until(&terminal, |state| {
            state.text().contains("hello") && state.exited
        });
        assert_eq!(state.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_output_uses_raw_byte_boundary() {
        require_live_pty!();
        let pty = PtyHandle::spawn_command(
            std::env::temp_dir(),
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "printf boundary".to_string()],
            "sh".to_string(),
        )
        .unwrap();
        let events = pty.events();
        let start = Instant::now();
        let mut output = Vec::new();
        let mut exited = false;
        while !exited {
            match events.try_recv() {
                Ok(PtyEvent::Output(bytes)) => output.extend(bytes),
                Ok(PtyEvent::Exited { .. }) => exited = true,
                Ok(PtyEvent::ProcessInfoChanged { .. }) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(start.elapsed() < Duration::from_secs(120));
                    thread::sleep(Duration::from_millis(10));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("PTY event stream closed before exit")
                }
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("boundary"));
    }

    #[cfg(unix)]
    #[test]
    fn pty_kill_emits_exit_data_event() {
        require_live_pty!();
        let pty = PtyHandle::spawn_command(
            std::env::temp_dir(),
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "sleep 10".to_string()],
            "sh".to_string(),
        )
        .unwrap();
        let events = pty.events();
        pty.kill().unwrap();
        let start = Instant::now();
        loop {
            match events.try_recv() {
                Ok(PtyEvent::Exited { .. }) => break,
                Ok(PtyEvent::Output(_) | PtyEvent::ProcessInfoChanged { .. }) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(
                        start.elapsed() < Duration::from_secs(120),
                        "PTY did not emit an exit event after kill"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("PTY event stream closed before kill exit")
                }
            }
        }
        assert!(pty.exited());
    }

    #[cfg(unix)]
    #[test]
    fn resizes_grid_and_pty() {
        require_live_pty!();
        let terminal = command("sleep 1");
        terminal.resize(42, 9);
        let state = terminal.snapshot();
        assert_eq!((state.cols, state.rows), (42, 9));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_input_and_emulator_replies() {
        require_live_pty!();
        let terminal = command("read line; printf '%s\\n' \"$line\"");
        terminal.write_input(b"echo tcode-term-ok\r".to_vec());
        let state = wait_until(&terminal, |state| {
            state.text().contains("echo tcode-term-ok")
        });
        assert!(state.text().contains("echo tcode-term-ok"));
    }

    #[cfg(unix)]
    #[test]
    fn handles_large_output_and_scrollback() {
        require_live_pty!();
        let terminal = command("seq 1 5000");
        let state = wait_until(&terminal, |state| {
            state.exited && state.text().contains("5000")
        });
        assert_eq!(state.exit_code, Some(0));
        terminal.scroll(800);
        let scrolled = terminal.snapshot();
        assert!(scrolled.display_offset > 0);
        assert!(!scrolled.text().contains("5000"));
    }

    #[cfg(unix)]
    #[test]
    fn programmatic_selection_returns_grid_text() {
        require_live_pty!();
        let terminal = command("printf 'alpha\\nbeta\\n'; sleep 1");
        let state = wait_until(&terminal, |state| state.text().contains("beta"));
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
        terminal.select((alpha_row, 0), (beta_row, 3));
        let selected = terminal.selected_text().unwrap();
        assert_eq!(selected.text, "alpha\nbeta");
        assert_eq!(selected.line_end, selected.line_start + 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pty_creation_is_serialized() {
        use std::sync::{
            Arc, Barrier,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        };

        use crate::pty::with_pty_creation;

        const THREADS: usize = 16;
        let start = Arc::new(Barrier::new(THREADS + 1));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = mpsc::channel();
        let mut threads = Vec::with_capacity(THREADS);

        for _ in 0..THREADS {
            let start = start.clone();
            let active = active.clone();
            let max_active = max_active.clone();
            let release = release.clone();
            let entered_tx = entered_tx.clone();
            threads.push(thread::spawn(move || {
                start.wait();
                with_pty_creation(|| {
                    let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now_active, Ordering::SeqCst);
                    entered_tx.send(()).unwrap();
                    while !release.load(Ordering::SeqCst) {
                        thread::yield_now();
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }));
        }
        drop(entered_tx);

        start.wait();
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("a thread should enter the PTY creation helper");
        let second_entry = entered_rx.recv_timeout(Duration::from_millis(250));
        release.store(true, Ordering::SeqCst);
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            max_active.load(Ordering::SeqCst),
            1,
            "unexpected concurrent helper entry: {second_entry:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn compatibility_events_preserve_grid_and_pty_sources() {
        require_live_pty!();
        let terminal = command("printf '\\033]2;wire-title\\007\\007'");
        let events = terminal.events();
        let start = Instant::now();
        let mut emitted = Vec::new();
        while !emitted.contains(&TermEvent::TitleChanged)
            || !emitted.contains(&TermEvent::Bell)
            || !emitted.contains(&TermEvent::Exited)
            || !emitted.contains(&TermEvent::Wakeup)
        {
            match events.try_recv() {
                Ok(event) => emitted.push(event),
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(
                        start.elapsed() < Duration::from_secs(120),
                        "missing compatibility events: {emitted:?}"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("compatibility event stream closed: {emitted:?}")
                }
            }
        }
        let state = terminal.snapshot();
        assert_eq!(state.title, "wire-title");
        assert!(state.exited);
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_mouse_mode_changes_routing_decision() {
        require_live_pty!();
        let terminal = command("printf '\\033[?1002h\\033[?1006h'; sleep 1");
        let state = wait_until(&terminal, |state| {
            state.mode.mouse_drag && state.mode.sgr_mouse
        });
        assert!(state.mode.routes_mouse(false));
        assert!(!state.mode.routes_mouse(true));
    }

    #[cfg(unix)]
    #[test]
    fn extracts_plain_and_osc8_hyperlinks() {
        require_live_pty!();
        let plain = command("printf 'see https://example.com/docs?q=1 now\\n'; sleep 1");
        let state = wait_until(&plain, |state| {
            state.text().contains("https://example.com/docs?q=1")
        });
        let index = state.cells.iter().position(|cell| cell.ch == 'h').unwrap();
        assert_eq!(
            plain
                .hyperlink_at(index / state.cols, index % state.cols + 10)
                .unwrap()
                .url,
            "https://example.com/docs?q=1"
        );

        let osc = command(
            "printf '\\033]8;;https://example.com/target\\033\\\\click-me\\033]8;;\\033\\\\'; sleep 1",
        );
        let state = wait_until(&osc, |state| state.text().contains("click-me"));
        let index = state.cells.iter().position(|cell| cell.ch == 'c').unwrap();
        assert_eq!(
            osc.hyperlink_at(index / state.cols, index % state.cols)
                .unwrap()
                .url,
            "https://example.com/target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_wide_cells_spacers_and_combining_characters() {
        require_live_pty!();
        let terminal = command("echo '中文e\u{301}'; sleep 1");
        let state = wait_until(&terminal, |state| state.text().contains("中文e\u{301}"));
        let first = state.cells.iter().position(|cell| cell.ch == '中').unwrap();
        let row = first / state.cols;
        let column = first % state.cols;
        assert!(state.cells[first].wide);
        assert!(state.cells[first + 1].wide_spacer);
        assert_eq!(state.cells[first + 4].text, "e\u{301}");

        terminal.select((row, column + 1), (row, column + 3));
        assert_eq!(terminal.selected_text().unwrap().text, "中文");
        let selected = terminal.snapshot();
        assert!(selected.cells[first].selected);
        assert!(selected.cells[first + 1].selected);
    }

    #[cfg(unix)]
    #[test]
    fn forwards_primary_device_attribute_response_to_pty() {
        require_live_pty!();
        let terminal = command(
            "saved=$(stty -g); stty raw -echo; printf '\\033[c'; response=$(dd bs=1 count=5 2>/dev/null); stty \"$saved\"; printf '%s' \"$response\" | od -An -tx1; printf '\\n'",
        );
        let state = wait_until(&terminal, |state| {
            let text = state.text();
            let fields = text.split_whitespace().collect::<Vec<_>>();
            state.exited
                && fields
                    .windows(5)
                    .any(|window| window == ["1b", "5b", "3f", "36", "63"])
        });
        assert_eq!(state.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn interactive_selection_and_clear_preserve_scrollback_semantics() {
        require_live_pty!();
        let terminal = command("printf 'alpha\\n'; sleep 1");
        let state = wait_until(&terminal, |state| state.text().contains("alpha"));
        let row = state
            .text()
            .lines()
            .position(|line| line.contains("alpha"))
            .unwrap();
        terminal.start_selection(SelectionKind::Simple, (row, 0), SelectionSide::Left);
        assert_eq!(terminal.selected_text(), None);
        terminal.update_selection((row, 4), SelectionSide::Right);
        assert_eq!(terminal.selected_text().unwrap().text, "alpha");

        terminal.clear_selection();
        terminal.select_all();
        assert!(
            terminal
                .selected_text()
                .is_some_and(|selection| selection.text.contains("alpha"))
        );
        terminal.clear();
        assert_eq!(terminal.snapshot().history_size, 0);
        assert_eq!(terminal.selected_text(), None);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "queries a real foreground PTY process group"]
    fn tracks_real_pty_foreground_cwd() {
        let terminal = command("cd /tmp && sleep 5");
        let expected = std::fs::canonicalize("/tmp").unwrap();
        let start = Instant::now();
        while terminal.working_directory() != expected {
            assert!(start.elapsed() < Duration::from_secs(10));
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_captures_output_resizes_and_accepts_input() {
        require_live_pty!();
        let output = command("echo hello");
        let state = wait_until(&output, |state| {
            state.text().contains("hello") && state.exited
        });
        assert_eq!(state.exit_code, Some(0));

        let resized = command("timeout /t 1 >nul");
        resized.resize(42, 9);
        assert_eq!((resized.snapshot().cols, resized.snapshot().rows), (42, 9));

        let input = command("set /p line= && echo %line%");
        input.write_input(b"tcode-term-ok\r".to_vec());
        let state = wait_until(&input, |state| state.text().contains("tcode-term-ok"));
        assert!(state.text().contains("tcode-term-ok"));
    }
}
