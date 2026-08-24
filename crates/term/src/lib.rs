//! UI-agnostic terminal process management and emulation.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    thread,
};

pub use rio_vt;
use rio_vt::{
    ansi::KeyboardModes,
    clipboard::ClipboardType,
    config::colors::{AnsiColor, ColorRgb},
    crosswords::{
        Mode,
        grid::{Indexed, row::Row},
        pos::{Column, CursorState, Line, Pos},
        square::{ContentTag, Square, Wide},
        style::Style,
    },
    event::TerminalDamage,
    selection::SelectionRange,
};

mod grid_emulator;
mod hyperlinks;
pub mod mappings;
mod pty;
mod pty_info;
mod sync;

pub use hyperlinks::HyperlinkMatch;

/// Renderer-facing graphics values and pure placement geometry from rio.
pub mod graphics {
    pub use rio_graphics::{
        ColorType, GraphicData, GraphicId, GraphicOverlay, atlas_image_key, kitty_image_key,
    };
    pub use rio_vt::ansi::graphics::{
        AtlasPlacement, KittyOverlayGeometry, KittyPlacement, OverlayViewport, UpdateQueues,
        VirtualPlacement, atlas_overlay_geometry, clip_overlay_to_rect, kitty_overlay_geometry,
        resolve_source_rect,
    };
    pub use rio_vt::ansi::kitty_virtual::{
        IncompletePlacement, PLACEHOLDER, PlaceholderRun, RunGeometry, compute_run_geometry,
    };
}

use grid_emulator::{GridEmulator, GridEvent};
use pty::{PtyEvent, PtyHandle};

const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;
const DEFAULT_CELL_WIDTH_PX: u32 = 8;
const DEFAULT_CELL_HEIGHT_PX: u32 = 17;

/// A rendering-relevant event emitted by the compatibility terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermEvent {
    /// The terminal grid or cursor changed.
    Wakeup,
    /// The emulated terminal rang the bell.
    Bell,
    /// The host-side child process exited.
    Exited,
    /// An OSC 52 request to store decoded UTF-8 text in a clipboard.
    ClipboardStore { kind: ClipboardType, text: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedText {
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct TermSnapshot {
    pub cols: usize,
    pub screen_lines: usize,
    pub visible_rows: Vec<Row<Square>>,
    pub row_damage: Vec<bool>,
    pub damage: TerminalDamage,
    pub cursor_state: CursorState,
    pub cursor: Option<(usize, usize)>,
    pub cursor_blinking: bool,
    pub title: String,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub display_offset: usize,
    pub history_size: usize,
    /// Lines evicted from rio's scrollback ring before the retained history.
    pub lines_evicted: u64,
    /// Image pixels added or removed since the preceding snapshot.
    ///
    /// Pixel buffers appear here once and are consumed by the renderer's image
    /// registry; placement snapshots below intentionally contain metadata only.
    pub graphics_updates: Option<graphics::UpdateQueues>,
    /// Active-screen sixel and iTerm2 placement metadata; geometry filters it to the viewport.
    pub atlas_placements: Vec<graphics::AtlasPlacement>,
    /// Active-screen direct kitty placement metadata, sorted by z-index.
    pub kitty_placements: Vec<graphics::KittyPlacement>,
    /// Kitty Unicode-placeholder placement metadata keyed by image/placement id.
    pub kitty_virtual_placements: HashMap<(u32, u32), graphics::VirtualPlacement>,
    pub mode: Mode,
    pub keyboard_mode: KeyboardModes,
    pub selection: Option<SelectionRange>,
    /// Snapshot of rio's interned style table, indexed by `Square::style_id`.
    pub styles: Vec<Style>,
    /// Zero-width continuations for visible squares, indexed by rio extras id.
    pub zero_width: HashMap<u16, Vec<char>>,
}

impl TermSnapshot {
    pub fn cell(&self, row: usize, col: usize) -> Option<&Square> {
        self.visible_rows.get(row)?.inner.get(col)
    }

    pub fn style(&self, row: usize, col: usize) -> Option<Style> {
        let square = *self.cell(row, col)?;
        Some(match square.content_tag() {
            ContentTag::Codepoint => self
                .styles
                .get(square.style_id() as usize)
                .copied()
                .unwrap_or_default(),
            ContentTag::BgPalette => Style {
                bg: AnsiColor::Indexed(square.bg_palette_index()),
                ..Style::default()
            },
            ContentTag::BgRgb => {
                let (r, g, b) = square.bg_rgb();
                Style {
                    bg: AnsiColor::Spec(ColorRgb { r, g, b }),
                    ..Style::default()
                }
            }
        })
    }

    /// The cell's base character followed by its rio zero-width continuation.
    pub fn cell_text(&self, row: usize, col: usize) -> Option<String> {
        let square = *self.cell(row, col)?;
        if square.content_tag() != ContentTag::Codepoint {
            return Some(" ".to_string());
        }
        let mut text = String::new();
        text.push(display_char(square.c()));
        if let Some(extra) = square.extras_id().and_then(|id| self.zero_width.get(&id)) {
            text.extend(extra.iter().copied().map(display_char));
        }
        Some(text)
    }

    pub fn is_selected(&self, row: usize, col: usize) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let Some(square) = self.cell(row, col) else {
            return false;
        };
        let pos = Pos::new(Line(row as i32 - self.display_offset as i32), Column(col));
        selection.contains_square(
            &Indexed { pos, square },
            self.cursor_state.pos,
            self.cursor_state.content,
        )
    }

    pub fn text(&self) -> String {
        self.visible_rows
            .iter()
            .enumerate()
            .map(|row| {
                let (row_index, row) = row;
                let text = row
                    .inner
                    .iter()
                    .take(self.cols)
                    .enumerate()
                    .filter(|(_, square)| square.wide() != Wide::Spacer)
                    .filter_map(|(col, _)| self.cell_text(row_index, col))
                    .collect::<String>();
                text.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn display_char(ch: char) -> char {
    if ch == '\0' { ' ' } else { ch }
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
                            let _ = pty_notifications.try_send(TermEvent::Wakeup);
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
                        GridEvent::TitleChanged(_) | GridEvent::CursorBlinkingChanged => {
                            Some(TermEvent::Wakeup)
                        }
                        GridEvent::Bell => Some(TermEvent::Bell),
                        GridEvent::Input(bytes) => {
                            writer.write_raw(bytes);
                            None
                        }
                        GridEvent::ClipboardStore { kind, text } => {
                            Some(TermEvent::ClipboardStore { kind, text })
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
            let _ = self.pty.resize(self.emulator.window_size());
        }
    }

    /// Update the physical pixel dimensions of one terminal cell.
    ///
    /// This updates rio's graphics sizing and the host PTY pixel winsize even
    /// when the grid's row and column counts stay unchanged.
    pub fn set_cell_size(&self, width_px: u32, height_px: u32) {
        if self.emulator.set_cell_size(width_px, height_px) {
            let _ = self.pty.resize(self.emulator.window_size());
        }
    }

    /// Resize the grid and update its physical cell metrics as one operation.
    pub fn resize_with_cell_size(&self, cols: usize, rows: usize, width_px: u32, height_px: u32) {
        if self
            .emulator
            .resize_with_cell_size_if_changed(cols, rows, width_px, height_px)
        {
            let _ = self.pty.resize(self.emulator.window_size());
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

    /// Read terminal modes without consuming renderer damage.
    pub fn mode(&self) -> Mode {
        self.emulator.mode()
    }

    /// Read kitty keyboard protocol flags without consuming renderer damage.
    pub fn keyboard_mode(&self) -> KeyboardModes {
        self.emulator.keyboard_mode()
    }

    /// Read the active xterm modifyOtherKeys level without consuming damage.
    pub fn modify_other_keys(&self) -> Option<u8> {
        self.emulator.modify_other_keys()
    }

    /// Read scrollback size without consuming renderer damage.
    pub fn history_size(&self) -> usize {
        self.emulator.history_size()
    }

    /// Read host process exit state without consuming renderer damage.
    pub fn exited(&self) -> bool {
        self.pty.exited()
    }

    pub fn snapshot(&self) -> TermSnapshot {
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

    fn wait_until(terminal: &Terminal, predicate: impl Fn(&TermSnapshot) -> bool) -> TermSnapshot {
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

    // Only the live-PTY tests use this, and those are unix-gated.
    #[cfg(unix)]
    fn find_char(state: &TermSnapshot, needle: char) -> Option<(usize, usize)> {
        state
            .visible_rows
            .iter()
            .enumerate()
            .find_map(|(row, cells)| {
                cells
                    .inner
                    .iter()
                    .take(state.cols)
                    .position(|square| square.c() == needle)
                    .map(|col| (row, col))
            })
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
        assert_eq!((state.cols, state.screen_lines), (42, 9));
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
    fn compatibility_events_cover_consumed_intents() {
        require_live_pty!();
        let terminal = command("printf '\\033]2;wire-title\\007\\007'");
        let events = terminal.events();
        let start = Instant::now();
        let mut emitted = Vec::new();
        while !emitted.contains(&TermEvent::Bell)
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
            state.mode.contains(Mode::MOUSE_DRAG) && state.mode.contains(Mode::SGR_MOUSE)
        });
        assert!(mappings::routes_mouse(state.mode, false));
        assert!(!mappings::routes_mouse(state.mode, true));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_exposes_active_kitty_keyboard_mode_without_a_snapshot() {
        require_live_pty!();
        let terminal = command("printf '\\033[>1u'; sleep 1");
        let start = Instant::now();
        while terminal.keyboard_mode() != KeyboardModes::DISAMBIGUATE_ESC_CODES {
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "terminal did not apply kitty keyboard mode"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn osc52_store_reaches_the_public_terminal_event_stream_decoded() {
        require_live_pty!();
        let terminal = command("printf '\\033]52;c;dGNvZGU=\\007'; sleep 1");
        let events = terminal.events();
        let start = Instant::now();
        loop {
            match events.try_recv() {
                Ok(TermEvent::ClipboardStore { kind, text }) => {
                    assert_eq!(kind, ClipboardType::Clipboard);
                    assert_eq!(text, "tcode");
                    break;
                }
                Ok(_) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(
                        start.elapsed() < Duration::from_secs(120),
                        "terminal did not emit the OSC 52 store"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("terminal event stream closed before the OSC 52 store")
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn extracts_plain_and_osc8_hyperlinks() {
        require_live_pty!();
        let plain = command("printf 'see https://example.com/docs?q=1 now\\n'; sleep 1");
        let state = wait_until(&plain, |state| {
            state.text().contains("https://example.com/docs?q=1")
        });
        let (row, col) = find_char(&state, 'h').unwrap();
        assert_eq!(
            plain.hyperlink_at(row, col + 10).unwrap().url,
            "https://example.com/docs?q=1"
        );

        let osc = command(
            "printf '\\033]8;;https://example.com/target\\033\\\\click-me\\033]8;;\\033\\\\'; sleep 1",
        );
        let state = wait_until(&osc, |state| state.text().contains("click-me"));
        let (row, col) = find_char(&state, 'c').unwrap();
        assert_eq!(
            osc.hyperlink_at(row, col).unwrap().url,
            "https://example.com/target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_wide_cells_spacers_and_combining_characters() {
        require_live_pty!();
        let terminal = command("echo '中文e\u{301}'; sleep 1");
        let state = wait_until(&terminal, |state| state.text().contains("中文e\u{301}"));
        let (row, column) = find_char(&state, '中').unwrap();
        assert_eq!(state.cell(row, column).unwrap().wide(), Wide::Wide);
        assert_eq!(state.cell(row, column + 1).unwrap().wide(), Wide::Spacer);
        assert_eq!(state.cell_text(row, column + 4).unwrap(), "e\u{301}");

        terminal.select((row, column + 1), (row, column + 3));
        assert_eq!(terminal.selected_text().unwrap().text, "中文");
        let selected = terminal.snapshot();
        assert!(selected.is_selected(row, column));
        assert!(selected.is_selected(row, column + 1));
    }

    #[cfg(unix)]
    #[test]
    fn forwards_primary_device_attribute_response_to_pty() {
        require_live_pty!();
        let terminal = command(
            "saved=$(stty -g); stty raw -echo; printf '\\033[c'; response=$(dd bs=1 count=16 2>/dev/null); stty \"$saved\"; printf '%s' \"$response\" | od -An -tx1; printf '\\n'",
        );
        let state = wait_until(&terminal, |state| {
            let text = state.text();
            let fields = text.split_whitespace().collect::<Vec<_>>();
            state.exited
                && fields.windows(16).any(|window| {
                    window
                        == [
                            "1b", "5b", "3f", "36", "32", "3b", "34", "3b", "36", "3b", "32", "32",
                            "3b", "35", "32", "63",
                        ]
                })
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
        assert_eq!(
            (resized.snapshot().cols, resized.snapshot().screen_lines),
            (42, 9)
        );

        let input = command("set /p line= && echo %line%");
        input.write_input(b"tcode-term-ok\r".to_vec());
        let state = wait_until(&input, |state| state.text().contains("tcode-term-ok"));
        assert!(state.text().contains("tcode-term-ok"));
    }
}
