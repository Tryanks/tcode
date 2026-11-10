//! The commands a thread runs through `tcode_tasks`: host processes bound to
//! the thread, not to the harness process that asked for them, with their
//! output spooled to a file under the host's data dir.
//!
//! A task's record is written beside its output when it starts and when it
//! ends. A host that opens the directory again finds every task its last run
//! left running, and marks it lost: the process went with that host
//! ([`stop_every_task`]), or at least nobody watches it any more.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// How long an ended task's record and output stay on disk.
const KEEP_ENDED: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long a stopped task has between SIGTERM and SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// Where a task stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Phase {
    Running,
    /// The process ended: with its exit code, or the signal that ended it.
    Exited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
        /// Unix ms.
        ended_at: u64,
    },
    /// The host that ran it went away while it ran; whether it still runs is
    /// unknown, and nothing reports its end.
    Lost,
}

impl Phase {
    pub fn running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// Who moved a foreground task to the background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mover {
    /// The user, from Tcode's UI.
    User,
    /// The model, with `task_background`.
    Agent,
    /// `task_run`'s `yield_after_s` elapsed.
    Yield,
}

/// What a waiter on a task sees change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskState {
    pub phase: Phase,
    /// A `task_run` call still waits on it.
    pub foreground: bool,
}

/// What is stored of a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    id: String,
    command: String,
    cwd: PathBuf,
    /// Unix ms.
    started_at: u64,
    #[serde(flatten)]
    phase: Phase,
    #[serde(default)]
    notify_on_exit: bool,
    /// A stop was sent; an exit after it is the stop's.
    #[serde(default)]
    stopped: bool,
    /// The stop was its deadline's.
    #[serde(default)]
    timed_out: bool,
}

/// A task as the tools and the host's UI read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskInfo {
    pub task_id: String,
    pub command: String,
    pub cwd: PathBuf,
    /// Unix ms.
    pub started_at: u64,
    #[serde(flatten)]
    pub phase: Phase,
    pub foreground: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_by: Option<Mover>,
    pub notify_on_exit: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stopped: bool,
    /// Stopped because its `timeout` ran out.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
    pub output_bytes: u64,
    /// The tool call that started it, when the host bound one.
    #[serde(skip)]
    pub item_id: Option<String>,
}

impl TaskInfo {
    /// How long it ran, or has run until `now` (unix ms).
    pub fn duration_ms(&self, now: u64) -> u64 {
        let end = match self.phase {
            Phase::Exited { ended_at, .. } => ended_at,
            _ => now,
        };
        end.saturating_sub(self.started_at)
    }
}

/// What the host hears from the manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    /// A session's list of tasks, or one task's standing, changed.
    Changed { session_id: String },
    /// A task's process ended.
    Exited { session_id: String, task_id: String },
}

/// One task to start.
#[derive(Debug, Clone)]
pub struct Spawn {
    pub command: String,
    pub cwd: PathBuf,
    /// A `task_run` call waits on it.
    pub foreground: bool,
    pub notify_on_exit: bool,
    pub item_id: Option<String>,
    /// The whole environment it runs with; `None` for the host's own.
    pub env: Option<BTreeMap<String, String>>,
    /// How long it may run before it is stopped, wherever it runs.
    pub timeout: Option<Duration>,
}

/// A window of a task's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// The last lines, at most `max_bytes` of them.
    Tail { lines: usize, max_bytes: usize },
    /// From byte `offset`, at most `max_bytes`.
    From { offset: u64, max_bytes: usize },
}

/// The output a [`Window`] holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Output {
    pub text: String,
    /// Byte offsets of `text` in the whole output.
    pub start: u64,
    pub end: u64,
    pub total_bytes: u64,
}

struct Task {
    record: Record,
    foreground: bool,
    moved_by: Option<Mover>,
    item_id: Option<String>,
    /// The model saw its end through a tool result, so no wake reports it.
    reported: bool,
    pid: Option<u32>,
    state: watch::Sender<TaskState>,
}

impl Task {
    fn new(record: Record, foreground: bool, item_id: Option<String>, pid: Option<u32>) -> Self {
        let (state, _) = watch::channel(TaskState {
            phase: record.phase.clone(),
            foreground,
        });
        Self {
            record,
            foreground,
            moved_by: None,
            item_id,
            reported: false,
            pid,
            state,
        }
    }

    fn publish(&self) {
        self.state.send_replace(TaskState {
            phase: self.record.phase.clone(),
            foreground: self.foreground,
        });
    }
}

#[derive(Default)]
struct SessionTasks {
    next: u64,
    tasks: Vec<Task>,
}

struct Inner {
    root: PathBuf,
    sessions: Mutex<HashMap<String, SessionTasks>>,
    events: async_channel::Sender<TaskEvent>,
}

/// Every task of every thread on this host.
#[derive(Clone)]
pub struct TaskManager {
    inner: Arc<Inner>,
}

/// Process groups of running tasks, by pid, for a host that goes away.
static LIVE: Mutex<BTreeMap<u32, ()>> = Mutex::new(BTreeMap::new());

fn live() -> MutexGuard<'static, BTreeMap<u32, ()>> {
    LIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Kill every task this process started, for a host that is going away.
pub fn stop_every_task() {
    for pid in live().keys() {
        signal_group(*pid, Signal::Kill);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// A session id is used as a directory name only when it is one plainly.
fn session_dir_name(session_id: &str) -> Option<&str> {
    (!session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then_some(session_id)
}

impl TaskManager {
    /// The tasks kept under `root`, every one left running marked lost, and
    /// the channel the manager reports changes on.
    pub fn open(root: PathBuf) -> (Self, async_channel::Receiver<TaskEvent>) {
        let (events, receiver) = async_channel::unbounded();
        let sessions = load(&root);
        (
            Self {
                inner: Arc::new(Inner {
                    root,
                    sessions: Mutex::new(sessions),
                    events,
                }),
            },
            receiver,
        )
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<String, SessionTasks>> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: TaskEvent) {
        let _ = self.inner.events.try_send(event);
    }

    fn dir(&self, session_id: &str) -> Result<PathBuf, String> {
        let name = session_dir_name(session_id).ok_or("unusable thread id")?;
        Ok(self.inner.root.join(name))
    }

    /// Start `spawn` for the thread `session_id`.
    pub fn spawn(&self, session_id: &str, spawn: Spawn) -> Result<TaskInfo, String> {
        let dir = self.dir(session_id)?;
        std::fs::create_dir_all(&dir).map_err(|error| format!("task directory: {error}"))?;
        let mut sessions = self.sessions();
        let session = sessions.entry(session_id.to_owned()).or_default();
        session.next += 1;
        let id = format!("t{}", session.next);
        let log_path = dir.join(format!("{id}.log"));
        let log = std::fs::File::create(&log_path)
            .map_err(|error| format!("task output file: {error}"))?;
        let errors = log
            .try_clone()
            .map_err(|error| format!("task output file: {error}"))?;
        let mut command = shell(&spawn.command);
        if let Some(env) = &spawn.env {
            command.env_clear().envs(env);
        }
        command
            .current_dir(&spawn.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(errors);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("could not start the command: {error}"))?;
        let pid = child.id();
        live().insert(pid, ());
        let record = Record {
            id: id.clone(),
            command: spawn.command,
            cwd: spawn.cwd,
            started_at: now_ms(),
            phase: Phase::Running,
            notify_on_exit: spawn.notify_on_exit,
            stopped: false,
            timed_out: false,
        };
        write_record(&dir, &record);
        let task = Task::new(record, spawn.foreground, spawn.item_id, Some(pid));
        let info = info(&task, &dir);
        session.tasks.push(task);
        drop(sessions);
        let weak = Arc::downgrade(&self.inner);
        let waited_session = session_id.to_owned();
        let waited = id.clone();
        let waiter = std::thread::Builder::new()
            .name(format!("tcode-task-{id}"))
            .spawn(move || {
                let status = child.wait();
                live().remove(&pid);
                if let Some(manager) = Weak::upgrade(&weak).map(|inner| TaskManager { inner }) {
                    manager.finish(&waited_session, &waited, status.ok());
                }
            });
        if let Err(error) = waiter {
            log::error!("tasks: no thread to wait on task {id}: {error}");
        }
        if let Some(timeout) = spawn.timeout {
            let weak = Arc::downgrade(&self.inner);
            let session_id = session_id.to_owned();
            let id = id.clone();
            let _ = std::thread::Builder::new()
                .name("tcode-task-deadline".into())
                .spawn(move || {
                    std::thread::sleep(timeout);
                    if let Some(manager) = Weak::upgrade(&weak).map(|inner| TaskManager { inner }) {
                        manager.time_out(&session_id, &id);
                    }
                });
        }
        self.emit(TaskEvent::Changed {
            session_id: session_id.to_owned(),
        });
        Ok(info)
    }

    fn finish(&self, session_id: &str, id: &str, status: Option<std::process::ExitStatus>) {
        let Ok(dir) = self.dir(session_id) else {
            return;
        };
        {
            let mut sessions = self.sessions();
            let Some(task) = sessions
                .get_mut(session_id)
                .and_then(|session| session.tasks.iter_mut().find(|task| task.record.id == id))
            else {
                return;
            };
            task.pid = None;
            task.foreground = false;
            task.record.phase = Phase::Exited {
                exit_code: status.and_then(|status| status.code()),
                signal: status.and_then(exit_signal),
                ended_at: now_ms(),
            };
            write_record(&dir, &task.record);
            task.publish();
        }
        self.emit(TaskEvent::Exited {
            session_id: session_id.to_owned(),
            task_id: id.to_owned(),
        });
        self.emit(TaskEvent::Changed {
            session_id: session_id.to_owned(),
        });
    }

    fn with_task<R>(
        &self,
        session_id: &str,
        id: &str,
        f: impl FnOnce(&mut Task, &Path) -> R,
    ) -> Result<R, String> {
        let dir = self.dir(session_id)?;
        let mut sessions = self.sessions();
        let task = sessions
            .get_mut(session_id)
            .and_then(|session| session.tasks.iter_mut().find(|task| task.record.id == id))
            .ok_or_else(|| format!("this thread has no task {id}"))?;
        Ok(f(task, &dir))
    }

    /// The thread's tasks, oldest first.
    pub fn list(&self, session_id: &str) -> Vec<TaskInfo> {
        let Ok(dir) = self.dir(session_id) else {
            return Vec::new();
        };
        self.sessions()
            .get(session_id)
            .map(|session| session.tasks.iter().map(|task| info(task, &dir)).collect())
            .unwrap_or_default()
    }

    pub fn get(&self, session_id: &str, id: &str) -> Result<TaskInfo, String> {
        self.with_task(session_id, id, |task, dir| info(task, dir))
    }

    /// What a waiter on the task watches.
    pub fn watch(&self, session_id: &str, id: &str) -> Result<watch::Receiver<TaskState>, String> {
        self.with_task(session_id, id, |task, _| task.state.subscribe())
    }

    /// The thread's tasks that run in the background, as its UI lists them.
    pub fn background(&self, session_id: &str) -> Vec<TaskInfo> {
        self.list(session_id)
            .into_iter()
            .filter(|task| task.phase.running() && !task.foreground)
            .collect()
    }

    /// Move a foreground task to the background: its `task_run` call returns,
    /// and its end wakes the thread.
    pub fn move_to_background(
        &self,
        session_id: &str,
        id: &str,
        by: Mover,
    ) -> Result<TaskInfo, String> {
        let moved = self.with_task(session_id, id, |task, dir| {
            if !task.record.phase.running() {
                return Err(format!("task {id} is no longer running"));
            }
            let moved = task.foreground;
            if moved {
                task.foreground = false;
                task.moved_by = Some(by);
                task.record.notify_on_exit = true;
                write_record(dir, &task.record);
                task.publish();
            }
            Ok((moved, info(task, dir)))
        })??;
        if moved.0 {
            self.emit(TaskEvent::Changed {
                session_id: session_id.to_owned(),
            });
        }
        Ok(moved.1)
    }

    /// The foreground task the tool call `item_id` waits on: the one bound to
    /// it, else the oldest one not bound to any call.
    pub fn foreground_for_item(&self, session_id: &str, item_id: &str) -> Option<String> {
        let sessions = self.sessions();
        let tasks = &sessions.get(session_id)?.tasks;
        let foreground = || {
            tasks
                .iter()
                .filter(|task| task.foreground && task.record.phase.running())
        };
        foreground()
            .find(|task| task.item_id.as_deref() == Some(item_id))
            .or_else(|| foreground().find(|task| task.item_id.is_none()))
            .map(|task| task.record.id.clone())
    }

    /// Stop the task: SIGTERM to its process group, then SIGKILL to what is
    /// left after a grace period. Its end is reported as for any exit.
    pub fn stop(&self, session_id: &str, id: &str) -> Result<TaskInfo, String> {
        let (pid, info) = self.with_task(session_id, id, |task, dir| {
            let pid = task.pid.filter(|_| task.record.phase.running());
            if pid.is_some() && !task.record.stopped {
                task.record.stopped = true;
                write_record(dir, &task.record);
            }
            (pid, info(task, dir))
        })?;
        if let Some(pid) = pid {
            signal_group(pid, Signal::Term);
            let weak = Arc::downgrade(&self.inner);
            let session_id = session_id.to_owned();
            let id = id.to_owned();
            let _ = std::thread::Builder::new()
                .name("tcode-task-stop".into())
                .spawn(move || {
                    std::thread::sleep(STOP_GRACE);
                    let Some(manager) = Weak::upgrade(&weak).map(|inner| TaskManager { inner })
                    else {
                        return;
                    };
                    // Only a process still unreaped is signalled, so its id
                    // cannot name another process by now.
                    let still = manager
                        .with_task(&session_id, &id, |task, _| task.pid)
                        .ok()
                        .flatten();
                    if still == Some(pid) {
                        signal_group(pid, Signal::Kill);
                    }
                });
        }
        Ok(info)
    }

    /// The task's deadline passed: it is stopped, as timed out, when it still
    /// runs.
    fn time_out(&self, session_id: &str, id: &str) {
        let running = self
            .with_task(session_id, id, |task, dir| {
                let running = task.record.phase.running() && task.pid.is_some();
                if running {
                    task.record.timed_out = true;
                    write_record(dir, &task.record);
                }
                running
            })
            .unwrap_or(false);
        if running {
            let _ = self.stop(session_id, id);
        }
    }

    /// Stop every running task of the thread, or only those a `task_run`
    /// call still waits on.
    pub fn stop_all(&self, session_id: &str, foreground_only: bool) -> Vec<String> {
        let ids: Vec<String> = self
            .list(session_id)
            .into_iter()
            .filter(|task| task.phase.running() && (task.foreground || !foreground_only))
            .map(|task| task.task_id)
            .collect();
        for id in &ids {
            let _ = self.stop(session_id, id);
        }
        ids
    }

    /// The model saw the task's end, so no wake reports it.
    pub fn mark_reported(&self, session_id: &str, id: &str) {
        let _ = self.with_task(session_id, id, |task, _| task.reported = true);
    }

    /// The task, when its end is still the thread's to hear of: it asked to
    /// be told, the model has not seen the end, and nothing told it yet. Once
    /// taken, it is not taken again.
    pub fn take_wake(&self, session_id: &str, id: &str) -> Option<TaskInfo> {
        self.with_task(session_id, id, |task, dir| {
            (task.record.notify_on_exit && !task.reported && !task.record.phase.running()).then(
                || {
                    task.reported = true;
                    info(task, dir)
                },
            )
        })
        .ok()
        .flatten()
    }

    /// A window of the task's output.
    /// The raw bytes of the task's output from `offset`, at most `max`.
    pub fn read_bytes(
        &self,
        session_id: &str,
        id: &str,
        offset: u64,
        max: usize,
    ) -> Result<Vec<u8>, String> {
        let path = self.with_task(session_id, id, |task, dir| log_path(dir, &task.record.id))?;
        let read = || -> std::io::Result<Vec<u8>> {
            let mut file = std::fs::File::open(&path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = Vec::new();
            file.take(max as u64).read_to_end(&mut bytes)?;
            Ok(bytes)
        };
        read().map_err(|error| format!("could not read the output: {error}"))
    }

    pub fn read(&self, session_id: &str, id: &str, window: Window) -> Result<Output, String> {
        let path = self.with_task(session_id, id, |task, dir| log_path(dir, &task.record.id))?;
        read_window(&path, window).map_err(|error| format!("could not read the output: {error}"))
    }

    /// The thread is gone: its tasks are stopped and their files removed.
    pub fn forget(&self, session_id: &str) {
        let removed = self.sessions().remove(session_id);
        for task in removed.into_iter().flat_map(|session| session.tasks) {
            if let Some(pid) = task.pid.filter(|_| task.record.phase.running()) {
                signal_group(pid, Signal::Kill);
            }
        }
        if let Ok(dir) = self.dir(session_id) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn info(task: &Task, dir: &Path) -> TaskInfo {
    let output_bytes = std::fs::metadata(log_path(dir, &task.record.id))
        .map(|meta| meta.len())
        .unwrap_or(0);
    TaskInfo {
        task_id: task.record.id.clone(),
        command: task.record.command.clone(),
        cwd: task.record.cwd.clone(),
        started_at: task.record.started_at,
        phase: task.record.phase.clone(),
        foreground: task.foreground,
        moved_by: task.moved_by,
        notify_on_exit: task.record.notify_on_exit,
        stopped: task.record.stopped,
        timed_out: task.record.timed_out,
        output_bytes,
        item_id: task.item_id.clone(),
    }
}

fn log_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.log"))
}

fn write_record(dir: &Path, record: &Record) {
    let path = dir.join(format!("{}.json", record.id));
    let written = serde_json::to_vec(record)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            let partial = path.with_extension("json.partial");
            std::fs::write(&partial, bytes)?;
            std::fs::rename(&partial, &path)
        });
    if let Err(error) = written {
        log::warn!("tasks: could not save {}: {error}", path.display());
    }
}

/// Every session's tasks kept under `root`: those left running are lost now,
/// and those ended long ago are deleted.
fn load(root: &Path) -> HashMap<String, SessionTasks> {
    let mut sessions = HashMap::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return sessions;
    };
    let cutoff = now_ms().saturating_sub(KEEP_ENDED.as_millis() as u64);
    for entry in entries.flatten() {
        let dir = entry.path();
        let Some(session_id) = dir
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(session_dir_name)
            .map(str::to_owned)
        else {
            continue;
        };
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut session = SessionTasks::default();
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(mut record) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
            else {
                continue;
            };
            if let Some(number) = record
                .id
                .strip_prefix('t')
                .and_then(|n| n.parse::<u64>().ok())
            {
                session.next = session.next.max(number);
            }
            match record.phase {
                Phase::Running => {
                    record.phase = Phase::Lost;
                    write_record(&dir, &record);
                }
                Phase::Exited { ended_at, .. } if ended_at < cutoff => {
                    let _ = std::fs::remove_file(&path);
                    let _ = std::fs::remove_file(log_path(&dir, &record.id));
                    continue;
                }
                Phase::Lost if record.started_at < cutoff => {
                    let _ = std::fs::remove_file(&path);
                    let _ = std::fs::remove_file(log_path(&dir, &record.id));
                    continue;
                }
                _ => {}
            }
            let mut task = Task::new(record, false, None, None);
            // What ended before this host started is not news to wake on.
            task.reported = true;
            session.tasks.push(task);
        }
        session.tasks.sort_by_key(|task| {
            (
                task.record.started_at,
                task.record.id.trim_start_matches('t').parse::<u64>().ok(),
            )
        });
        if session.tasks.is_empty() {
            let _ = std::fs::remove_dir(&dir);
        } else {
            sessions.insert(session_id, session);
        }
    }
    sessions
}

fn read_window(path: &Path, window: Window) -> std::io::Result<Output> {
    let mut file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    let (start, max_bytes, tail_lines) = match window {
        Window::Tail { lines, max_bytes } => (
            total.saturating_sub(max_bytes as u64),
            max_bytes,
            Some(lines),
        ),
        Window::From { offset, max_bytes } => (offset.min(total), max_bytes, None),
    };
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(max_bytes.min(total as usize));
    file.take(max_bytes as u64).read_to_end(&mut bytes)?;
    let mut start = start;
    if let Some(lines) = tail_lines {
        // Keep whole lines: drop a first line the byte budget cut, then all
        // but the last `lines`.
        let mut from = 0;
        if start > 0
            && let Some(newline) = bytes.iter().position(|b| *b == b'\n')
        {
            from = newline + 1;
        }
        let body = &bytes[from..];
        let trimmed = body.strip_suffix(b"\n").unwrap_or(body);
        let mut seen = 0;
        let mut cut = 0;
        for (index, byte) in trimmed.iter().enumerate().rev() {
            if *byte == b'\n' {
                seen += 1;
                if seen == lines {
                    cut = index + 1;
                    break;
                }
            }
        }
        from += cut;
        start += from as u64;
        bytes.drain(..from);
    }
    let end = start + bytes.len() as u64;
    Ok(Output {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        start,
        end,
        total_bytes: total,
    })
}

fn shell(command: &str) -> std::process::Command {
    #[cfg(unix)]
    {
        let program = if Path::new("/bin/bash").exists() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };
        let mut shell = agent::command(program);
        shell.arg("-c").arg(command);
        shell
    }
    #[cfg(not(unix))]
    {
        let mut shell = agent::command("cmd");
        shell.arg("/C").arg(command);
        shell
    }
}

#[cfg(unix)]
fn exit_signal(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// Signal the process group `pid` leads (every task leads its own).
#[cfg(unix)]
fn signal_group(pid: u32, signal: Signal) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    let signal = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: signalling the group a task's unreaped child leads.
    unsafe { libc::killpg(pid, signal) };
}

#[cfg(not(unix))]
fn signal_group(pid: u32, _signal: Signal) {
    let _ = agent::command("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
