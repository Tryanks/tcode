//! Child-process constructors that never flash a console window on Windows,
//! and never hand a child this process's own standard handles.
//!
//! Provider CLIs are spawned from a GUI app; a plain `Command` on Windows
//! allocates a console for the child, which pops a black box on screen.
//! `CREATE_NO_WINDOW` suppresses it. No-ops elsewhere.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use smol::channel::Receiver;

use crate::AgentError;

const STDERR_TAIL_LINES: usize = 20;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Keep this process's own standard handles out of every child it starts.
///
/// Windows gives a child each inheritable handle of its parent, not only the
/// stdio its `Command` names, so a child that outlives the parent keeps open
/// the pipes whoever runs the parent reads from. A child spawned with
/// `Stdio::inherit` still gets them: std hands it an inheritable duplicate.
/// No-op elsewhere.
fn keep_own_stdio_from_children() {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        use windows::Win32::Foundation::{
            HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
        };
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            for handle in [
                std::io::stdin().as_raw_handle(),
                std::io::stdout().as_raw_handle(),
                std::io::stderr().as_raw_handle(),
            ] {
                // SAFETY: these are this process's standard handles, open for
                // its lifetime; a missing one only makes the call fail, which
                // leaves nothing to keep from a child.
                let _ = unsafe {
                    SetHandleInformation(HANDLE(handle), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
                };
            }
        });
    }
}

/// A blocking `std::process::Command` with the console suppressed.
///
/// On Windows a package manager's `.cmd` shim is started as what it runs
/// (see `cmd_shim`), so its arguments never pass through `cmd.exe`.
pub fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    keep_own_stdio_from_children();
    let program = program.as_ref();
    let shim = if cfg!(windows) {
        crate::cmd_shim::launch(std::path::Path::new(program))
    } else {
        None
    };
    // Approved constructor: this is the helper every other site must use.
    #[allow(clippy::disallowed_methods)]
    let mut cmd = std::process::Command::new(
        shim.as_ref()
            .map_or(program, |shim| shim.program.as_os_str()),
    );
    if let Some(shim) = shim {
        cmd.args(shim.args);
        if let Some(node_path) = shim.node_path {
            cmd.env("NODE_PATH", node_path);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// [`command`] for a harness CLI: on Unix the child leads a process group of
/// its own, which [`ProcessTree::kill`] stops with it.
pub(crate) fn harness_command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut cmd = command(program);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    cmd
}

/// [`harness_command`] as a `smol` command.
pub(crate) fn async_harness_command<S: AsRef<std::ffi::OsStr>>(
    program: S,
) -> smol::process::Command {
    smol::process::Command::from(harness_command(program))
}

/// A child whose process tree [`ProcessTree`] owns.
pub(crate) trait Leader {
    fn id(&self) -> u32;
    fn kill(&mut self) -> std::io::Result<()>;
}

impl Leader for std::process::Child {
    fn id(&self) -> u32 {
        std::process::Child::id(self)
    }
    fn kill(&mut self) -> std::io::Result<()> {
        std::process::Child::kill(self)
    }
}

impl Leader for smol::process::Child {
    fn id(&self) -> u32 {
        smol::process::Child::id(self)
    }
    fn kill(&mut self) -> std::io::Result<()> {
        smol::process::Child::kill(self)
    }
}

/// A harness child and every process it starts, stopped together.
///
/// A harness starts its own background work as further processes, and Claude
/// Code 2.1.318 starts each shell detached, in a session of its own, so
/// killing the harness alone leaves that work running with nobody to answer
/// to. [`kill`](Self::kill), dropping the tree, and [`stop_every_tree`] stop
/// all of it. On Windows the tree is a job object closed with the process; on
/// Unix it is read from the parent links before anything is killed, since an
/// orphan's parent link is lost.
pub(crate) struct ProcessTree<C: Leader> {
    leader: C,
    entry: u64,
}

static LIVE_TREES: Mutex<std::collections::BTreeMap<u64, TreeHandle>> =
    Mutex::new(std::collections::BTreeMap::new());
static NEXT_TREE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(unix)]
type TreeHandle = u32;
#[cfg(windows)]
struct TreeHandle(windows::Win32::Foundation::HANDLE);
#[cfg(not(any(unix, windows)))]
type TreeHandle = ();

// SAFETY: a job handle is a kernel handle usable from any thread; only the
// registry closes it, once it is taken out under the lock.
#[cfg(windows)]
unsafe impl Send for TreeHandle {}

fn live_trees() -> std::sync::MutexGuard<'static, std::collections::BTreeMap<u64, TreeHandle>> {
    LIVE_TREES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<C: Leader> ProcessTree<C> {
    pub(crate) fn new(leader: C) -> Self {
        let entry = NEXT_TREE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = tree_handle(leader.id()) {
            live_trees().insert(entry, handle);
        }
        Self { leader, entry }
    }

    /// Stop the harness and everything it started.
    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        if let Some(handle) = live_trees().get(&self.entry) {
            stop_tree(handle);
        }
        self.leader.kill()
    }
}

impl<C: Leader> std::ops::Deref for ProcessTree<C> {
    type Target = C;
    fn deref(&self) -> &C {
        &self.leader
    }
}

impl<C: Leader> std::ops::DerefMut for ProcessTree<C> {
    fn deref_mut(&mut self) -> &mut C {
        &mut self.leader
    }
}

impl<C: Leader> Drop for ProcessTree<C> {
    fn drop(&mut self) {
        let _ = self.kill();
        if let Some(handle) = live_trees().remove(&self.entry) {
            release_tree(handle);
        }
    }
}

/// The harness processes one session starts, which whoever started the
/// session stops without going through its actor: an actor waiting on its
/// harness reads no command until the harness answers, a session still
/// starting has no actor to ask, and the harness's answer is what the user
/// stopped waiting for. A tree held after [`stop`](Self::stop) is stopped as
/// it is held.
#[derive(Clone, Default, Debug)]
pub struct SessionProcesses(Arc<Mutex<HeldTrees>>);

#[derive(Default, Debug)]
struct HeldTrees {
    stopped: bool,
    entries: Vec<u64>,
}

impl SessionProcesses {
    fn held(&self) -> std::sync::MutexGuard<'_, HeldTrees> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hold `tree` as one of this session's.
    pub(crate) fn hold<C: Leader>(&self, mut tree: ProcessTree<C>) -> ProcessTree<C> {
        let stopped = {
            let mut held = self.held();
            if !held.stopped {
                held.entries.push(tree.entry);
            }
            held.stopped
        };
        if stopped {
            let _ = tree.kill();
        }
        tree
    }

    /// Stop every tree this session holds, and each it holds from now on.
    pub fn stop(&self) {
        let entries = {
            let mut held = self.held();
            held.stopped = true;
            std::mem::take(&mut held.entries)
        };
        let trees = live_trees();
        for entry in entries {
            if let Some(handle) = trees.get(&entry) {
                stop_tree(handle);
            }
        }
    }
}

/// Stop every harness tree this process started, for a host that is going
/// away: the actors its shutdown asked to stop may not run again before the
/// process exits.
pub fn stop_every_tree() {
    for handle in live_trees().values() {
        stop_tree(handle);
    }
}

#[cfg(unix)]
fn tree_handle(pid: u32) -> Option<TreeHandle> {
    Some(pid)
}

#[cfg(windows)]
fn tree_handle(pid: u32) -> Option<TreeHandle> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
    // SAFETY: plain Win32 calls on handles this function owns; every handle
    // is closed on each path that does not hand it to the registry.
    unsafe {
        let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let assigned = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
        .and_then(|()| OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid))
        .and_then(|process| {
            let assigned = AssignProcessToJobObject(job, process);
            let _ = CloseHandle(process);
            assigned
        });
        if let Err(error) = assigned {
            log::warn!("harness process {pid} runs outside a job object: {error}");
            let _ = CloseHandle(job);
            return None;
        }
        Some(TreeHandle(job))
    }
}

#[cfg(not(any(unix, windows)))]
fn tree_handle(_pid: u32) -> Option<TreeHandle> {
    None
}

#[cfg(windows)]
fn stop_tree(job: &TreeHandle) {
    // SAFETY: the registry keeps the job handle open while it is listed.
    let _ = unsafe { windows::Win32::System::JobObjects::TerminateJobObject(job.0, 1) };
}

#[cfg(windows)]
fn release_tree(job: TreeHandle) {
    // SAFETY: the handle was just taken out of the registry, its only owner.
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(job.0) };
}

#[cfg(not(windows))]
fn release_tree(_handle: TreeHandle) {}

#[cfg(not(any(unix, windows)))]
fn stop_tree(_handle: &TreeHandle) {}

/// Stop `leader` and its descendants: each is stopped as it is found, so none
/// starts another while the tree is read, and then all are killed, with every
/// process group one of them leads, which holds what they orphaned.
#[cfg(unix)]
fn stop_tree(leader: &TreeHandle) {
    let Ok(leader) = libc::pid_t::try_from(*leader) else {
        return;
    };
    // Once reaped, the id may name an unrelated process.
    if !unreaped_child(leader) {
        return;
    }
    let mut tree = vec![leader];
    // SAFETY: signalling a process id this process still holds unreaped.
    unsafe { libc::kill(leader, libc::SIGSTOP) };
    for _ in 0..16 {
        let found: Vec<_> = descendants(leader)
            .into_iter()
            .filter(|pid| !tree.contains(pid))
            .collect();
        if found.is_empty() {
            break;
        }
        for &pid in &found {
            // SAFETY: a descendant of a stopped, unreaped child of this
            // process, which nothing else can reap and reuse before the kill.
            unsafe { libc::kill(pid, libc::SIGSTOP) };
        }
        tree.extend(found);
    }
    // SAFETY: reading this process's own group.
    let own_group = unsafe { libc::getpgrp() };
    for &pid in &tree {
        // SAFETY: as above; a group is signalled only when one of the tree's
        // processes leads it.
        if pid != own_group && unsafe { libc::getpgid(pid) } == pid {
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
    }
    for &pid in &tree {
        // SAFETY: as above.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// Whether `pid` is a child of this process that has not been reaped yet,
/// leaving it unreaped.
#[cfg(unix)]
fn unreaped_child(pid: libc::pid_t) -> bool {
    // SAFETY: `info` is a valid, writable siginfo; WNOWAIT leaves any exit
    // status in place for the child's owner.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == 0
    }
}

#[cfg(target_vendor = "apple")]
fn descendants(root: libc::pid_t) -> Vec<libc::pid_t> {
    let mut found = Vec::new();
    let mut pending = vec![root];
    while let Some(parent) = pending.pop() {
        let mut children = vec![0 as libc::pid_t; 1024];
        // SAFETY: the buffer holds `children.len()` pids and its size in
        // bytes is passed with it.
        let count = unsafe {
            libc::proc_listchildpids(
                parent,
                children.as_mut_ptr().cast(),
                std::mem::size_of_val(children.as_slice()) as libc::c_int,
            )
        };
        let count = usize::try_from(count).unwrap_or(0).min(children.len());
        for &child in &children[..count] {
            if child > 0 && !found.contains(&child) {
                found.push(child);
                pending.push(child);
            }
        }
    }
    found
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn descendants(root: libc::pid_t) -> Vec<libc::pid_t> {
    let mut parents = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        // The parent is the second field after the name, which closes with
        // the line's last `)`.
        let Some(parent) = std::fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|stat| {
                let (_, fields) = stat.rsplit_once(')')?;
                fields
                    .split_whitespace()
                    .nth(1)?
                    .parse::<libc::pid_t>()
                    .ok()
            })
        else {
            continue;
        };
        parents.push((pid, parent));
    }
    let mut found = Vec::new();
    let mut pending = vec![root];
    while let Some(parent) = pending.pop() {
        for &(pid, _) in parents.iter().filter(|(_, of)| *of == parent) {
            if !found.contains(&pid) {
                found.push(pid);
                pending.push(pid);
            }
        }
    }
    found
}

#[cfg(all(
    unix,
    not(any(target_vendor = "apple", target_os = "linux", target_os = "android"))
))]
fn descendants(_root: libc::pid_t) -> Vec<libc::pid_t> {
    Vec::new()
}

/// Refuses an argument holding a line break for a Windows batch file, which
/// `cmd.exe` would cut short there; std refuses it too, in words that name
/// neither the program nor a way out.
pub(crate) fn refuse_batch_line_breaks<'a>(
    program: &std::ffi::OsStr,
    mut args: impl Iterator<Item = &'a std::ffi::OsStr>,
) -> Result<(), AgentError> {
    let program = std::path::Path::new(program);
    let batch = program
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        });
    if cfg!(windows)
        && batch
        && args.any(|arg| {
            arg.as_encoded_bytes()
                .iter()
                .any(|byte| matches!(byte, b'\r' | b'\n'))
        })
    {
        return Err(AgentError::BatchLineBreak {
            program: program.to_path_buf(),
        });
    }
    Ok(())
}

/// Run a command to its end and collect what it wrote. Its stdin is null, so
/// a prompt can never wait on input; dropping the future stops the command
/// and everything it started, as a cancelled plugin change or harness update
/// does.
pub async fn tree_output(command: std::process::Command) -> std::io::Result<std::process::Output> {
    held_output(command, &SessionProcesses::default()).await
}

/// [`tree_output`] for a command a session runs before its harness: the
/// session's stop kills it as it kills the harness.
pub(crate) async fn held_output(
    mut command: std::process::Command,
    processes: &SessionProcesses,
) -> std::io::Result<std::process::Output> {
    use smol::io::AsyncReadExt as _;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    async fn read_all(pipe: Option<impl smol::io::AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut bytes).await?;
        }
        Ok(bytes)
    }
    let mut child = processes.hold(ProcessTree::new(
        smol::process::Command::from(command).spawn()?,
    ));
    let (stdout, stderr) =
        smol::future::try_zip(read_all(child.stdout.take()), read_all(child.stderr.take())).await?;
    let status = child.status().await?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Runs blocking work on `smol`'s blocking pool. The one approved
/// `smol::unblock` site in this crate, so callers are easy to audit.
#[allow(clippy::disallowed_methods)]
pub(crate) fn unblock<T, F>(f: F) -> smol::Task<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    smol::unblock(f)
}

pub(crate) async fn probe_version(
    binary: &std::path::Path,
    launch_env: &crate::LaunchEnv,
    provider: crate::ProviderKind,
    processes: &SessionProcesses,
) -> Option<(u32, u32, u32)> {
    let mut command = command(binary);
    command.arg("--version");
    if provider == crate::ProviderKind::ClaudeCode {
        command
            .env_remove("CLAUDECODE")
            .env_remove("CLAUDE_CODE_ENTRYPOINT");
    }
    for (key, value) in launch_env.pairs(provider) {
        command.env(key, value);
    }
    let output = held_output(command, processes).await.ok()?;
    crate::parse_semver(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) enum ChildOutput {
    Line(String),
    Eof,
    Error(String),
}

pub(crate) fn spawn_line_reader(
    stdout: impl Read + Send + 'static,
    thread_name: &str,
    error_prefix: Option<&'static str>,
    skip_empty: bool,
) -> (Receiver<ChildOutput>, std::io::Result<()>) {
    let (sender, receiver) = smol::channel::unbounded();
    let spawned = std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if (!skip_empty || !line.trim().is_empty())
                            && sender.send_blocking(ChildOutput::Line(line)).is_err()
                        {
                            return;
                        }
                    }
                    Err(err) => {
                        let error = match error_prefix {
                            Some(prefix) => format!("{prefix}: {err}"),
                            None => err.to_string(),
                        };
                        let _ = sender.send_blocking(ChildOutput::Error(error));
                        return;
                    }
                }
            }
            let _ = sender.send_blocking(ChildOutput::Eof);
        })
        .map(|_| ());
    (receiver, spawned)
}

pub(crate) fn send_json(
    writer: &mut impl Write,
    value: &Value,
    serialization_context: Option<&str>,
) -> Result<(), AgentError> {
    serde_json::to_writer(&mut *writer, value).map_err(|err| {
        AgentError::Protocol(match serialization_context {
            Some(context) => format!("{context}: {err}"),
            None => err.to_string(),
        })
    })?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// A rolling tail of process output used to enrich startup and exit errors.
#[derive(Clone, Default)]
pub(crate) struct StderrTail {
    lines: Arc<Mutex<Vec<String>>>,
    readers: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}

impl StderrTail {
    pub(crate) fn spawn(
        &self,
        reader: impl Read + Send + 'static,
        thread_name: &str,
        log_prefix: &'static str,
    ) -> std::io::Result<()> {
        let tail = self.clone();
        let reader = std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                for line in BufReader::new(reader).lines().map_while(Result::ok) {
                    log::debug!("{log_prefix}: {line}");
                    tail.push(line);
                }
            })?;
        self.readers.lock().unwrap().push(reader);
        Ok(())
    }

    pub(crate) fn push(&self, line: String) {
        let mut lines = self.lines.lock().unwrap();
        if lines.len() == STDERR_TAIL_LINES {
            lines.remove(0);
        }
        lines.push(line);
    }

    /// Joins spawned readers; stop or reap their child before collecting diagnostics.
    pub(crate) fn append_to(&self, mut message: String, separator: &str) -> String {
        let tail = self.text();
        if !tail.trim().is_empty() {
            message.push_str(separator);
            message.push_str(&tail);
        }
        message
    }

    fn text(&self) -> String {
        let readers = std::mem::take(&mut *self.readers.lock().unwrap());
        for reader in readers {
            let _ = reader.join();
        }
        self.lines.lock().unwrap().join("\n")
    }
}

#[cfg(test)]
pub(crate) const TEST_ECHO_READY: &str = "TCODE_TEST_ECHO_READY";

#[cfg(test)]
pub(crate) fn test_echo_command() -> std::process::Command {
    // Reuse the test executable so actor fixtures need no shell or PATH tools.
    let mut child = command(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "process::tests::echo_child",
            "--quiet",
            "--nocapture",
        ])
        .env("TCODE_TEST_ECHO_CHILD", "1");
    child
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code starts a background shell in a session of its own, out of
    /// its process group; stopping the harness stops that shell too. The
    /// harness and its work each hold the test's pipe until they exit, and
    /// each waits on stdin, which the test holds open, so neither ends on its
    /// own while the test runs.
    #[cfg(unix)]
    #[test]
    fn stopping_a_harness_stops_the_work_it_started_out_of_its_group() {
        const ROLE: &str = "TCODE_TEST_TREE_ROLE";
        let this_test = |mut child: std::process::Command, role: &str| {
            child
                .args([
                    "--exact",
                    "process::tests::stopping_a_harness_stops_the_work_it_started_out_of_its_group",
                    "--quiet",
                    "--nocapture",
                ])
                .env(ROLE, role);
            child
        };
        let exe = std::env::current_exe().unwrap();
        let wait_for_stdin = || std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        match std::env::var(ROLE).as_deref() {
            Ok("work") => {
                let _ = wait_for_stdin();
                return;
            }
            Ok("harness") => {
                use std::os::unix::process::CommandExt as _;
                let mut work = this_test(command(exe), "work")
                    .process_group(0)
                    .spawn()
                    .unwrap();
                println!("work {}", work.id());
                let _ = wait_for_stdin();
                let _ = work.kill();
                let _ = work.wait();
                return;
            }
            _ => {}
        }

        let mut harness = ProcessTree::new(
            this_test(harness_command(exe), "harness")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let _stdin = harness.stdin.take();
        let mut output = BufReader::new(harness.stdout.take().unwrap()).lines();
        let work = output
            .find_map(|line| line.unwrap().strip_prefix("work ")?.parse::<u32>().ok())
            .unwrap();

        harness.kill().unwrap();
        harness.wait().unwrap();

        let (closed, everyone_gone) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            output.for_each(drop);
            let _ = closed.send(());
        });
        // Work that outlives the harness holds its stdout open, and the test
        // runner ends the wait.
        everyone_gone
            .recv()
            .unwrap_or_else(|_| panic!("the harness's background work {work} outlived it"));
    }

    /// A session's harness that waits on its stdin, which the test holds open,
    /// ends only when the session is stopped; one started after the stop does
    /// not outlive its start.
    #[test]
    fn stopping_a_session_kills_its_harness_whatever_it_waits_on() {
        const ROLE: &str = "TCODE_TEST_SESSION_HARNESS";
        if std::env::var_os(ROLE).is_some() {
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
            return;
        }
        let harness = || {
            let mut child = harness_command(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "process::tests::stopping_a_session_kills_its_harness_whatever_it_waits_on",
                    "--quiet",
                ])
                .env(ROLE, "1")
                .stdin(std::process::Stdio::piped());
            ProcessTree::new(child.spawn().unwrap())
        };
        let processes = SessionProcesses::default();
        let mut running = processes.hold(harness());
        let _stdin = running.stdin.take();

        processes.stop();
        assert!(!running.wait().unwrap().success());

        let mut late = processes.hold(harness());
        let _stdin = late.stdin.take();
        assert!(!late.wait().unwrap().success());
    }

    /// A plugin change the user cancels drops its command's future, which
    /// stops the command. The command here holds a pipe the test reads, which
    /// ends only when it exits.
    #[cfg(unix)]
    #[test]
    fn dropping_a_management_command_stops_it() {
        const ROLE: &str = "TCODE_TEST_PARKED_COMMAND";
        if let Some(fifo) = std::env::var_os(ROLE) {
            let mut held = std::fs::OpenOptions::new().write(true).open(fifo).unwrap();
            writeln!(held, "parked").unwrap();
            loop {
                std::thread::park();
            }
        }
        let fifo = std::env::temp_dir().join(format!("tcode-parked-{}", uuid::Uuid::new_v4()));
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path this test owns.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let mut command = command(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "process::tests::dropping_a_management_command_stops_it",
                "--quiet",
            ])
            .env(ROLE, &fifo);
        let running = smol::spawn(tree_output(command));
        let mut held = BufReader::new(std::fs::File::open(&fifo).unwrap());
        let mut line = String::new();
        held.read_line(&mut line).unwrap();
        assert_eq!(line, "parked\n");

        drop(running);
        held.read_to_string(&mut line).unwrap();
        std::fs::remove_file(&fifo).unwrap();
    }

    /// Not a test: the echo child [`test_echo_command`] starts, which writes
    /// back each line it reads. Run by the test runner, it does nothing.
    #[test]
    fn echo_child() {
        if std::env::var_os("TCODE_TEST_ECHO_CHILD").is_none() {
            return;
        }
        let mut output = std::io::stdout().lock();
        writeln!(output, "{TEST_ECHO_READY}").unwrap();
        output.flush().unwrap();
        for line in std::io::stdin().lock().lines() {
            writeln!(output, "{}", line.unwrap()).unwrap();
            output.flush().unwrap();
        }
    }
}
