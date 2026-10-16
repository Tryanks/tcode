//! Provider processes end with the host that started them, however it ends.
//!
//! A provider cannot tell that its host is gone: Claude Code keeps executing
//! its turn after stdin closes and exits only once its queued input is done,
//! and OpenCode's server has no stdin at all. An aborted or killed host runs no
//! `Drop`, and macOS has no parent-death signal, so the tie lives outside the
//! host. On Unix a watchdog shell joins the provider's own process group and
//! reads a pipe only the host holds; when the pipe closes, because the host let
//! go or died, it sends SIGTERM to the group (Claude Code then stops its tool
//! shells, which run in sessions of their own) and SIGKILL after a grace. Its
//! membership keeps the group id from being recycled, so neither signal can
//! reach an unrelated process. On Windows a kill-on-close job object does it.

use std::fs::File;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::AgentError;

/// Bounds a later host's wait for an earlier host's providers. Claude Code
/// exits within a second of SIGTERM; the watchdog's own grace is three seconds.
const LEASE_WAIT: Duration = Duration::from_secs(15);

/// `$1` is the provider's pid, which is also the group id.
#[cfg(unix)]
const WATCHDOG: &str = "trap '' TERM; read -r line; kill -TERM 0; i=0; \
     while kill -0 \"$1\" 2>/dev/null && [ \"$i\" -lt 30 ]; do sleep 0.1; i=$((i + 1)); done; \
     kill -KILL 0";

/// A data directory's claim on the provider processes started for it.
///
/// A host takes the claim before its first provider starts, which waits until
/// no provider started by an earlier host on the same data directory is still
/// alive. Every watchdog the host starts shares the claim, so a host that
/// follows waits for those watchdogs, and with them their providers, in turn.
#[derive(Clone, Debug)]
pub struct ProviderLease(Arc<Lease>);

#[derive(Debug)]
struct Lease {
    path: PathBuf,
    held: Mutex<Option<Arc<File>>>,
}

impl ProviderLease {
    pub fn new(data_dir: &Path) -> Self {
        Self(Arc::new(Lease {
            path: data_dir.join("providers.lock"),
            held: Mutex::default(),
        }))
    }

    /// Blocks until the claim is held.
    pub(crate) fn hold(&self) -> Result<Arc<File>, AgentError> {
        let mut held = self
            .0
            .held
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(file) = &*held {
            return Ok(file.clone());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.0.path)?;
        let deadline = Instant::now() + LEASE_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(AgentError::Spawn(format!(
                        "a provider started by an earlier Tcode host is still running ({} is \
                         held)",
                        self.0.path.display()
                    )));
                }
            }
        }
        let file = Arc::new(file);
        *held = Some(file.clone());
        Ok(file)
    }
}

/// A provider child together with what ends it when the host does. Spawn it
/// from [`super::provider_command`] or [`super::provider_async_command`]: the
/// watchdog can only join a group the provider leads.
pub(crate) struct Tied<C> {
    child: C,
    _tie: HostTie,
}

impl<C> Deref for Tied<C> {
    type Target = C;

    fn deref(&self) -> &C {
        &self.child
    }
}

impl<C> DerefMut for Tied<C> {
    fn deref_mut(&mut self) -> &mut C {
        &mut self.child
    }
}

pub(crate) trait HostChild {
    fn pid(&self) -> u32;
    #[cfg(windows)]
    fn raw_handle(&self) -> std::os::windows::io::RawHandle;
}

impl HostChild for std::process::Child {
    fn pid(&self) -> u32 {
        self.id()
    }
    #[cfg(windows)]
    fn raw_handle(&self) -> std::os::windows::io::RawHandle {
        std::os::windows::io::AsRawHandle::as_raw_handle(self)
    }
}

impl HostChild for smol::process::Child {
    fn pid(&self) -> u32 {
        self.id()
    }
    #[cfg(windows)]
    fn raw_handle(&self) -> std::os::windows::io::RawHandle {
        std::os::windows::io::AsRawHandle::as_raw_handle(self)
    }
}

/// Ties `child` to this host. The provider keeps running untied when the tie
/// cannot be made; that is logged rather than failing the session.
pub(crate) fn tie<C: HostChild>(child: C, lease: Option<&ProviderLease>) -> Tied<C> {
    let tie = HostTie::new(&child, lease).unwrap_or_else(|error| {
        log::warn!(
            "provider process {} is not tied to this host: {error}",
            child.pid()
        );
        HostTie::default()
    });
    Tied { child, _tie: tie }
}

#[derive(Default)]
struct HostTie {
    /// Dropping it closes the watchdog's stdin; the child is reaped in the
    /// background once the watchdog exits.
    #[cfg(unix)]
    _watchdog: Option<smol::process::Child>,
}

impl HostTie {
    #[cfg(unix)]
    fn new(child: &impl HostChild, lease: Option<&ProviderLease>) -> Result<Self, AgentError> {
        use std::os::unix::process::CommandExt as _;
        use std::process::Stdio;

        let pid = child.pid();
        let claim = lease.map(ProviderLease::hold).transpose()?;
        let mut command = super::command("/bin/sh");
        command
            .arg("-c")
            .arg(WATCHDOG)
            .arg("tcode-provider-watchdog")
            .arg(pid.to_string())
            .process_group(pid as i32);
        let mut command = smol::process::Command::from(command);
        // The claim rides on the watchdog's stdout, which it never writes.
        let stdout = match claim {
            Some(claim) => Stdio::from(claim.try_clone()?),
            None => Stdio::null(),
        };
        command
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(Stdio::null());
        Ok(Self {
            _watchdog: Some(command.spawn()?),
        })
    }

    #[cfg(windows)]
    fn new(child: &impl HostChild, _lease: Option<&ProviderLease>) -> Result<Self, AgentError> {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        let job = host_job().ok_or_else(|| {
            AgentError::Spawn(format!(
                "no job object: {}",
                std::io::Error::last_os_error()
            ))
        })?;
        // SAFETY: `job` is this process's job handle, never closed, and the
        // child's handle is open for as long as `child` is borrowed.
        if unsafe { AssignProcessToJobObject(job as _, child.raw_handle() as _) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self::default())
    }

    #[cfg(not(any(unix, windows)))]
    fn new(_child: &impl HostChild, _lease: Option<&ProviderLease>) -> Result<Self, AgentError> {
        Ok(Self::default())
    }
}

/// One job per host process. Its handle is closed only by the host's exit,
/// which is what terminates every process assigned to it.
#[cfg(windows)]
fn host_job() -> Option<usize> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    static JOB: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *JOB.get_or_init(|| {
        // SAFETY: plain Win32 calls on a handle this function owns; the
        // limit structure outlives the call that reads it.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            (set != 0).then_some(job as usize)
        }
    })
}
