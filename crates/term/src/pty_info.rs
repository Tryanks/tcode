use std::{
    fs::File,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::fd::AsRawFd as _;
#[cfg(unix)]
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub name: String,
    pub cwd: PathBuf,
}

pub(crate) struct PtyInfo {
    #[cfg(unix)]
    file: File,
    #[cfg(unix)]
    fallback_pid: u32,
    last_refresh: Mutex<Option<Instant>>,
}

impl PtyInfo {
    #[cfg(unix)]
    pub fn new(file: File, fallback_pid: u32) -> Self {
        Self {
            file,
            fallback_pid,
            last_refresh: Mutex::new(None),
        }
    }

    #[cfg(not(unix))]
    pub fn new() -> Self {
        Self {
            last_refresh: Mutex::new(None),
        }
    }

    pub fn should_refresh(&self) -> bool {
        let mut last = self.last_refresh.lock().unwrap();
        if last.is_some_and(|time| time.elapsed() < Duration::from_millis(250)) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    #[cfg(unix)]
    pub fn load(&self) -> Option<ProcessInfo> {
        let foreground = unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) };
        let pid = if foreground > 0 {
            foreground as u32
        } else {
            self.fallback_pid
        };
        if pid == 0 {
            return None;
        }
        let pid = Pid::from_u32(pid);
        let refresh = ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_exe(UpdateKind::Always);
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), false, refresh);
        let process = system.process(pid)?;
        Some(ProcessInfo {
            name: process.name().to_string_lossy().into_owned(),
            cwd: process.cwd()?.to_path_buf(),
        })
    }

    #[cfg(not(unix))]
    pub fn load(&self) -> Option<ProcessInfo> {
        None
    }
}
