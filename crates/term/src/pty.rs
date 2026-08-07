//! Host-side pseudoterminal ownership and raw byte transport.

use std::{
    collections::{HashMap, VecDeque},
    io::{self, ErrorKind, Read as _, Write as _},
    num::NonZeroUsize,
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
    event::{OnResize as _, WindowSize},
    tty::{self, ChildEvent, EventedPty as _, EventedReadWrite as _},
};
use polling::{Event as PollingEvent, Events, PollMode, Poller};

use crate::{DEFAULT_COLS, DEFAULT_ROWS, pty_info};

const READ_BUFFER_SIZE: usize = 0x10_0000;
const PTY_CHILD_EVENT_TOKEN: usize = 1;
#[cfg(unix)]
const PTY_READ_WRITE_TOKEN: usize = 0;
#[cfg(windows)]
const PTY_READ_WRITE_TOKEN: usize = 2;

thread_local! {
    static SPAWN_CWD_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Data emitted by the host-side PTY.
///
/// Receivers see raw child output before terminal emulation. Foreground process
/// metadata and exit status are separate data events so they can cross the same
/// transport boundary without exposing platform process or PTY types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PtyEvent {
    Output(Vec<u8>),
    ProcessInfoChanged {
        name: String,
        working_directory: PathBuf,
    },
    Exited {
        exit_code: Option<i32>,
    },
}

struct Shared {
    process_name: Option<String>,
    working_directory: Option<PathBuf>,
    exited: bool,
    exit_code: Option<i32>,
    command_line: String,
    command_label: Option<String>,
}

/// Host-side handle for a child process running in a pseudoterminal.
///
/// This type owns process lifecycle and byte I/O only. It has no terminal-grid
/// state and its public API contains no Alacritty grid or parser types.
pub(crate) struct PtyHandle {
    sender: PtyCommandSender,
    notifications: async_channel::Sender<PtyEvent>,
    events: async_channel::Receiver<PtyEvent>,
    shared: Arc<Mutex<Shared>>,
    shell_name: String,
    cwd: PathBuf,
    pty_info: Arc<pty_info::PtyInfo>,
    refresh_running: Arc<AtomicBool>,
}

impl PtyHandle {
    /// Resolve the cwd that a subsequent [`PtyHandle::spawn`] should use.
    ///
    /// Call this before moving PTY creation to another thread so the
    /// thread-local test/UI override is captured on the calling thread.
    pub fn resolve_spawn_cwd(cwd: impl AsRef<Path>) -> PathBuf {
        SPAWN_CWD_OVERRIDE
            .with(|override_cwd| override_cwd.borrow().clone())
            .unwrap_or_else(|| cwd.as_ref().to_path_buf())
    }

    /// Spawn the platform's default interactive shell in `cwd`.
    pub fn spawn(cwd: impl AsRef<Path>) -> io::Result<Self> {
        let (shell, args) = default_shell();
        let shell_name = shell_label(&shell);
        let cwd = Self::resolve_spawn_cwd(cwd);
        Self::spawn_command(cwd, shell, args, shell_name)
    }

    pub(crate) fn spawn_command(
        cwd: impl AsRef<Path>,
        program: String,
        args: Vec<String>,
        shell_name: String,
    ) -> io::Result<Self> {
        let cwd = cwd.as_ref().to_path_buf();
        let size = PtySize {
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
        let pty = with_pty_creation(|| tty::new(&options, size.window_size(), 0))?;
        #[cfg(unix)]
        let pty_info = Arc::new(pty_info::PtyInfo::new(
            pty.file().try_clone()?,
            pty.child().id(),
        ));
        #[cfg(not(unix))]
        let pty_info = Arc::new(pty_info::PtyInfo::new());
        let shared = Arc::new(Mutex::new(Shared {
            process_name: None,
            working_directory: Some(cwd.clone()),
            exited: false,
            exit_code: None,
            command_line: String::new(),
            command_label: None,
        }));
        let refresh_running = Arc::new(AtomicBool::new(false));
        let (notifications, events) = async_channel::unbounded();
        let (sender, receiver) = mpsc::channel();
        let poller = Arc::new(Poller::new()?);
        let command_sender = PtyCommandSender {
            sender,
            poller: poller.clone(),
        };
        let event_loop = RawPtyEventLoop {
            pty,
            poller,
            receiver,
            notifications: notifications.clone(),
            shared: shared.clone(),
            pty_info: pty_info.clone(),
            refresh_running: refresh_running.clone(),
            drain_on_exit: options.drain_on_exit,
        };
        thread::Builder::new()
            .name("tcode-pty-io".into())
            .spawn(move || event_loop.run())?;

        schedule_process_refresh(
            pty_info.clone(),
            shared.clone(),
            notifications.clone(),
            refresh_running.clone(),
        );

        Ok(Self {
            sender: command_sender,
            notifications,
            events,
            shared,
            shell_name,
            cwd,
            pty_info,
            refresh_running,
        })
    }

    /// Apply a cwd override to [`PtyHandle::spawn`] calls made synchronously by `f`.
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

    /// Return the raw PTY event stream.
    ///
    /// Cloned receivers compete for events. A transport should install one
    /// draining task and fan out from there when multiple consumers are needed.
    pub fn events(&self) -> async_channel::Receiver<PtyEvent> {
        self.events.clone()
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

    pub fn label(&self) -> String {
        self.shared
            .lock()
            .unwrap()
            .process_name
            .clone()
            .unwrap_or_else(|| self.shell_name.clone())
    }

    pub fn exited(&self) -> bool {
        self.shared.lock().unwrap().exited
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.shared.lock().unwrap().exit_code
    }

    pub(crate) fn write_input_inner(&self, bytes: Vec<u8>) -> io::Result<bool> {
        let label_changed = {
            let mut shared = self.shared.lock().unwrap();
            let previous_label = shared.command_label.clone();
            track_command_input(&mut shared, &bytes);
            shared.command_label != previous_label
        };
        let result = self.sender.send(PtyCommand::Input(bytes));
        schedule_process_refresh(
            self.pty_info.clone(),
            self.shared.clone(),
            self.notifications.clone(),
            self.refresh_running.clone(),
        );
        result.map(|()| label_changed)
    }

    /// Write terminal-protocol response bytes without tracking them as user input.
    pub fn write_raw(&self, bytes: impl Into<Vec<u8>>) -> io::Result<()> {
        self.sender.send(PtyCommand::Input(bytes.into()))
    }

    /// Resize the host PTY. Dimensions are clamped to at least 2×2.
    pub fn resize(&self, cols: usize, rows: usize) -> io::Result<()> {
        self.sender.send(PtyCommand::Resize(PtySize {
            cols: cols.max(2),
            rows: rows.max(2),
        }))
    }

    /// Terminate the child process.
    pub fn kill(&self) -> io::Result<()> {
        self.sender.send(PtyCommand::Kill)
    }

    pub(crate) fn writer(&self) -> PtyWriter {
        PtyWriter {
            sender: self.sender.clone(),
        }
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        let _ = self.sender.send(PtyCommand::Shutdown);
    }
}

#[derive(Clone)]
pub(crate) struct PtyWriter {
    sender: PtyCommandSender,
}

impl PtyWriter {
    pub(crate) fn write_raw(&self, bytes: Vec<u8>) {
        let _ = self.sender.send(PtyCommand::Input(bytes));
    }
}

#[derive(Clone, Copy)]
struct PtySize {
    cols: usize,
    rows: usize,
}

impl PtySize {
    fn window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.rows as u16,
            num_cols: self.cols as u16,
            cell_width: 8,
            cell_height: 17,
        }
    }
}

enum PtyCommand {
    Input(Vec<u8>),
    Resize(PtySize),
    Kill,
    Shutdown,
}

#[derive(Clone)]
struct PtyCommandSender {
    sender: mpsc::Sender<PtyCommand>,
    poller: Arc<Poller>,
}

impl PtyCommandSender {
    fn send(&self, command: PtyCommand) -> io::Result<()> {
        self.sender
            .send(command)
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "PTY event loop has stopped"))?;
        self.poller.notify()
    }
}

struct RawPtyEventLoop {
    pty: tty::Pty,
    poller: Arc<Poller>,
    receiver: mpsc::Receiver<PtyCommand>,
    notifications: async_channel::Sender<PtyEvent>,
    shared: Arc<Mutex<Shared>>,
    pty_info: Arc<pty_info::PtyInfo>,
    refresh_running: Arc<AtomicBool>,
    drain_on_exit: bool,
}

impl RawPtyEventLoop {
    fn run(mut self) {
        let poll_mode = PollMode::Level;
        let mut interest = PollingEvent::readable(0);
        if unsafe { self.pty.register(&self.poller, interest, poll_mode) }.is_err() {
            return;
        }

        let mut events = Events::with_capacity(NonZeroUsize::new(1024).unwrap());
        let mut writes = VecDeque::new();
        let mut current_write = None;
        let mut buffer = vec![0; READ_BUFFER_SIZE];

        'event_loop: loop {
            events.clear();
            match self.poller.wait(&mut events, None) {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }

            if !self.drain_commands(&mut writes) {
                break;
            }

            for event in events.iter() {
                match event.key {
                    PTY_CHILD_EVENT_TOKEN => {
                        if let Some(ChildEvent::Exited(exit_code)) = self.pty.next_child_event() {
                            if self.drain_on_exit {
                                let _ = self.read_output(&mut buffer);
                            }
                            self.record_exit(exit_code);
                            break 'event_loop;
                        }
                    }
                    PTY_READ_WRITE_TOKEN => {
                        if event.is_interrupt() {
                            continue;
                        }
                        if event.readable
                            && let Err(_error) = self.read_output(&mut buffer)
                        {
                            #[cfg(target_os = "linux")]
                            if _error.raw_os_error() == Some(libc::EIO) {
                                continue;
                            }
                            break 'event_loop;
                        }
                        if event.writable
                            && Self::write_pending(&mut self.pty, &mut writes, &mut current_write)
                                .is_err()
                        {
                            break 'event_loop;
                        }
                    }
                    _ => {}
                }
            }

            let needs_write = current_write.is_some() || !writes.is_empty();
            if needs_write != interest.writable {
                interest.writable = needs_write;
                if self
                    .pty
                    .reregister(&self.poller, interest, poll_mode)
                    .is_err()
                {
                    break;
                }
            }
        }

        let _ = self.pty.deregister(&self.poller);
    }

    fn drain_commands(&mut self, writes: &mut VecDeque<Vec<u8>>) -> bool {
        loop {
            match self.receiver.try_recv() {
                Ok(PtyCommand::Input(bytes)) => {
                    if !bytes.is_empty() {
                        writes.push_back(bytes);
                    }
                }
                Ok(PtyCommand::Resize(size)) => self.pty.on_resize(size.window_size()),
                Ok(PtyCommand::Kill) => {
                    let _ = terminate_pty(&self.pty);
                }
                Ok(PtyCommand::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => return false,
                Err(mpsc::TryRecvError::Empty) => return true,
            }
        }
    }

    fn read_output(&mut self, buffer: &mut [u8]) -> io::Result<()> {
        let mut read_any = false;
        loop {
            match self.pty.reader().read(buffer) {
                Ok(0) => break,
                Ok(read) => {
                    read_any = true;
                    let _ = self
                        .notifications
                        .try_send(PtyEvent::Output(buffer[..read].to_vec()));
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
                {
                    if error.kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if read_any {
            schedule_process_refresh(
                self.pty_info.clone(),
                self.shared.clone(),
                self.notifications.clone(),
                self.refresh_running.clone(),
            );
        }
        Ok(())
    }

    fn write_pending(
        pty: &mut tty::Pty,
        writes: &mut VecDeque<Vec<u8>>,
        current: &mut Option<PendingWrite>,
    ) -> io::Result<()> {
        if current.is_none() {
            *current = writes.pop_front().map(PendingWrite::new);
        }
        while let Some(pending) = current.as_mut() {
            match pty.writer().write(pending.remaining()) {
                Ok(0) => return Ok(()),
                Ok(written) => {
                    pending.written += written;
                    if pending.remaining().is_empty() {
                        *current = writes.pop_front().map(PendingWrite::new);
                    }
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
                {
                    if error.kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn record_exit(&self, exit_code: Option<i32>) {
        let mut shared = self.shared.lock().unwrap();
        shared.exited = true;
        shared.exit_code = exit_code;
        shared.command_label = None;
        drop(shared);
        let _ = self.notifications.try_send(PtyEvent::Exited { exit_code });
    }
}

struct PendingWrite {
    bytes: Vec<u8>,
    written: usize,
}

impl PendingWrite {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, written: 0 }
    }

    fn remaining(&self) -> &[u8] {
        &self.bytes[self.written..]
    }
}

#[cfg(unix)]
fn terminate_pty(pty: &tty::Pty) -> io::Result<()> {
    let result = unsafe { libc::kill(pty.child().id() as i32, libc::SIGHUP) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn terminate_pty(pty: &tty::Pty) -> io::Result<()> {
    let result = unsafe {
        windows_sys::Win32::System::Threading::TerminateProcess(pty.child_watcher().raw_handle(), 1)
    };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn refresh_process_info(
    info: &pty_info::PtyInfo,
    shared: &Mutex<Shared>,
    notifications: &async_channel::Sender<PtyEvent>,
) {
    if let Some(process) = info.load() {
        let mut shared = shared.lock().unwrap();
        let changed = shared.process_name.as_deref() != Some(&process.name)
            || shared.working_directory.as_ref() != Some(&process.cwd);
        shared.process_name = Some(process.name.clone());
        shared.working_directory = Some(process.cwd.clone());
        drop(shared);
        if changed {
            let _ = notifications.try_send(PtyEvent::ProcessInfoChanged {
                name: process.name,
                working_directory: process.cwd,
            });
        }
    }
}

fn schedule_process_refresh(
    info: Arc<pty_info::PtyInfo>,
    shared: Arc<Mutex<Shared>>,
    notifications: async_channel::Sender<PtyEvent>,
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
            // Input/output can precede the shell applying `cd` or launching
            // its foreground process. One bounded trailing refresh catches
            // that transition even when the command itself is silent.
            thread::sleep(Duration::from_millis(300));
            refresh_process_info(&info, &shared, &notifications);
            running.store(false, Ordering::Release);
        });
    if spawn_result.is_err() {
        running_on_error.store(false, Ordering::Release);
    }
}

/// Derive the compact tab label used after a command is submitted.
fn derive_command_label(command: &str) -> Option<String> {
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

/// The interactive shell to spawn, as `(program, args)`.
pub(crate) fn default_shell() -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string());
        (shell, Vec::new())
    }
    #[cfg(not(windows))]
    {
        let shell = unix_shell(std::env::var("SHELL").ok().as_deref());
        (shell, vec!["-l".to_string()])
    }
}

#[cfg(not(windows))]
pub(crate) fn unix_shell(configured_shell: Option<&str>) -> String {
    configured_shell
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or(if cfg!(target_os = "macos") {
            "/bin/zsh"
        } else {
            "/bin/sh"
        })
        .to_string()
}

pub(crate) fn shell_label(shell: &str) -> String {
    let name = shell
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("shell");
    name.rsplit_once('.')
        .map_or(name, |(stem, _)| stem)
        .to_string()
}

#[cfg(target_os = "macos")]
pub(crate) fn with_pty_creation<T>(create: impl FnOnce() -> T) -> T {
    // Concurrent macOS openpty(3) calls can return an invalid negative errno.
    // Serialize only PTY creation process-wide; the terminal lifetime remains unlocked.
    static PTY_CREATION_LOCK: Mutex<()> = Mutex::new(());
    let guard = PTY_CREATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let result = create();
    drop(guard);
    result
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn with_pty_creation<T>(create: impl FnOnce() -> T) -> T {
    create()
}
