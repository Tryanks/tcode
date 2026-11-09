//! Who may use a machine's SSH pipe: only the Zed that Tcode launched for
//! it. A connection to the pipe is traced back through its processes —
//! `tcode ssh-proxy` started by `ssh` started directly by Zed's main process
//! — and admitted for a machine only within the window that opens when the
//! user asks Tcode to open the machine in Zed, after which that Zed alone is
//! bound to the machine until it exits.
use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};

/// How long after Open in Zed the launched Zed has to connect.
pub(super) const WINDOW: Duration = Duration::from_secs(60);

/// One process, as the operating system reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Process {
    pub(super) parent: Option<u32>,
    pub(super) exe: Option<std::path::PathBuf>,
    pub(super) started: Option<u64>,
}

/// What the pipe asks the operating system about the processes behind a
/// connection.
pub(super) trait Processes {
    fn process(&self, pid: u32) -> Option<Process>;
    /// Whether `pid` is a Zed main process by the platform's code identity.
    fn is_zed(&self, pid: u32) -> bool;
}

/// A process, by pid and start, so a later process reusing the pid is a
/// different one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProcessKey {
    pub(super) pid: u32,
    pub(super) started: Option<u64>,
}

impl ProcessKey {
    pub(super) fn alive(&self, processes: &impl Processes) -> bool {
        processes
            .process(self.pid)
            .is_some_and(|process| process.started == self.started)
    }
}

/// Why a connection was turned away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The chain behind the connection is not Tcode's proxy under ssh under
    /// Zed.
    NotZed,
    /// A Zed, but not the one Tcode opened this machine in.
    NotGranted,
}

/// The Zed behind the proxy process `proxy`: the proxy must be this Tcode
/// executable, its parent `ssh`, and ssh's parent Zed's main process itself,
/// so a shell in Zed's terminal, an agent, or anything else that runs ssh
/// does not qualify.
pub(super) fn zed_behind(
    processes: &impl Processes,
    proxy: u32,
    own_exe: &Path,
) -> Result<ProcessKey, Refusal> {
    let proxy = processes.process(proxy).ok_or(Refusal::NotZed)?;
    if proxy.exe.as_deref() != Some(own_exe) {
        return Err(Refusal::NotZed);
    }
    let ssh_pid = proxy.parent.ok_or(Refusal::NotZed)?;
    let ssh = processes.process(ssh_pid).ok_or(Refusal::NotZed)?;
    let is_ssh = ssh
        .exe
        .as_deref()
        .and_then(Path::file_stem)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("ssh"));
    if !is_ssh {
        return Err(Refusal::NotZed);
    }
    let zed_pid = ssh.parent.ok_or(Refusal::NotZed)?;
    let zed = processes.process(zed_pid).ok_or(Refusal::NotZed)?;
    if !processes.is_zed(zed_pid) {
        return Err(Refusal::NotZed);
    }
    Ok(ProcessKey {
        pid: zed_pid,
        started: zed.started,
    })
}

#[derive(Debug, Clone, Copy)]
enum Grant {
    /// Open in Zed was chosen; the first Zed to connect takes the machine.
    Open {
        until: Instant,
    },
    Bound(ProcessKey),
}

/// Each machine's grant.
#[derive(Default)]
pub(super) struct Grants {
    machines: HashMap<String, Grant>,
}

impl Grants {
    /// The user chose Open in Zed for `machine`. A Zed already bound to it
    /// stays bound while it runs: that is the Zed the request goes to.
    pub(super) fn open(&mut self, machine: &str, now: Instant, processes: &impl Processes) {
        if let Some(Grant::Bound(zed)) = self.machines.get(machine)
            && zed.alive(processes)
        {
            return;
        }
        self.machines.insert(
            machine.to_owned(),
            Grant::Open {
                until: now + WINDOW,
            },
        );
    }

    /// Admit `zed` to `machine`'s pipe, binding it when the window is open.
    pub(super) fn admit(
        &mut self,
        machine: &str,
        zed: ProcessKey,
        now: Instant,
        processes: &impl Processes,
    ) -> Result<(), Refusal> {
        match self.machines.get(machine).copied() {
            Some(Grant::Bound(bound)) if bound == zed => Ok(()),
            Some(Grant::Bound(bound)) if bound.alive(processes) => Err(Refusal::NotGranted),
            Some(Grant::Open { until }) if now < until => {
                self.machines.insert(machine.to_owned(), Grant::Bound(zed));
                Ok(())
            }
            _ => {
                self.machines.remove(machine);
                Err(Refusal::NotGranted)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A process table: pid → (parent, executable, is Zed).
    struct Table(Vec<(u32, u32, &'static str, bool)>);

    impl Processes for Table {
        fn process(&self, pid: u32) -> Option<Process> {
            self.0
                .iter()
                .find(|(id, ..)| *id == pid)
                .map(|(id, parent, exe, _)| Process {
                    parent: Some(*parent),
                    exe: Some(PathBuf::from(exe)),
                    started: Some(u64::from(*id) * 10),
                })
        }

        fn is_zed(&self, pid: u32) -> bool {
            self.0.iter().any(|(id, .., zed)| *id == pid && *zed)
        }
    }

    const TCODE: &str = "/Applications/Tcode.app/Contents/MacOS/tcode";
    const ZED: &str = "/Applications/Zed.app/Contents/MacOS/zed";

    #[test]
    fn only_ssh_started_by_zed_itself_reaches_a_machine_and_only_the_zed_tcode_opened() {
        let tcode = Path::new(TCODE);
        let table = Table(vec![
            (1, 0, "/sbin/launchd", false),
            // Zed's own ssh.
            (100, 1, ZED, true),
            (101, 100, "/usr/bin/ssh", false),
            (102, 101, TCODE, false),
            // ssh from a shell in Zed's terminal.
            (110, 100, "/bin/zsh", false),
            (111, 110, "/usr/bin/ssh", false),
            (112, 111, TCODE, false),
            // Something else posing as the proxy under Zed's ssh.
            (120, 101, "/tmp/tcode", false),
            // An impostor named zed that is not Zed by its signature.
            (130, 1, "/tmp/Zed.app/Contents/MacOS/zed", false),
            (131, 130, "/usr/bin/ssh", false),
            (132, 131, TCODE, false),
            // A second, genuine Zed.
            (140, 1, ZED, true),
            (141, 140, "/usr/bin/ssh", false),
            (142, 141, TCODE, false),
        ]);
        let zed = zed_behind(&table, 102, tcode).unwrap();
        assert_eq!(zed.pid, 100);
        for proxy in [112, 120, 132, 999] {
            assert_eq!(
                zed_behind(&table, proxy, tcode),
                Err(Refusal::NotZed),
                "{proxy}"
            );
        }

        let mut grants = Grants::default();
        let now = Instant::now();
        assert_eq!(
            grants.admit("m", zed, now, &table),
            Err(Refusal::NotGranted),
            "no Open in Zed yet"
        );
        grants.open("m", now, &table);
        assert_eq!(
            grants.admit("other", zed, now, &table),
            Err(Refusal::NotGranted),
            "the window is per machine"
        );
        assert_eq!(grants.admit("m", zed, now, &table), Ok(()));
        let second = zed_behind(&table, 142, tcode).unwrap();
        assert_eq!(
            grants.admit("m", second, now, &table),
            Err(Refusal::NotGranted),
            "the first Zed took the machine"
        );
        grants.open("m", now, &table);
        assert_eq!(
            grants.admit("m", second, now, &table),
            Err(Refusal::NotGranted),
            "opening again keeps the running Zed bound"
        );
        assert_eq!(
            grants.admit("m", zed, now + WINDOW * 10, &table),
            Ok(()),
            "the bound Zed stays admitted after the window"
        );

        // The bound Zed exits; a late connection after the window finds
        // nothing, and the next Open in Zed lets the other Zed take it.
        let without_first = Table(table.0.iter().copied().filter(|p| p.0 != 100).collect());
        assert_eq!(
            grants.admit("m", second, now + WINDOW * 2, &without_first),
            Err(Refusal::NotGranted)
        );
        grants.open("m", now + WINDOW * 2, &without_first);
        assert_eq!(
            grants.admit("m", second, now + WINDOW * 2, &without_first),
            Ok(())
        );
        let later = now + WINDOW * 4;
        grants.open("x", later, &without_first);
        assert_eq!(
            grants.admit("x", second, later + WINDOW, &without_first),
            Err(Refusal::NotGranted),
            "the window closes after a minute"
        );
    }
}
