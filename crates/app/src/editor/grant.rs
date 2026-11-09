//! Who may use a machine's SSH pipe: the Zed that Tcode launched for it.
//!
//! A connection to the pipe is traced back through its processes: `tcode
//! ssh-proxy`, started by `ssh`, started directly by Zed's main process as
//! the platform identifies Zed. Where Zed opens a machine through one
//! ControlMaster connection (macOS and Linux), the ssh must also be such a
//! master, which a command typed into Zed's terminal or run by its tasks is
//! not unless it imitates one. On Windows Zed's own ssh commands carry
//! nothing a command started directly by Zed would not. A connection is
//! admitted for a machine only within the window that opens when the user
//! asks Tcode to open the machine in Zed; that Zed then keeps the machine
//! until it exits. Programs running as the same user can imitate any of
//! this; the pipe does not stand between a user and their own programs.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How long after Open in Zed the launched Zed has to connect.
pub(super) const WINDOW: Duration = Duration::from_secs(60);

/// One process behind a connection, as the operating system reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Process {
    pub(super) pid: u32,
    pub(super) exe: Option<PathBuf>,
    /// `None` where the platform does not tell.
    pub(super) args: Option<Vec<String>>,
    pub(super) started: Option<u64>,
    /// Whether the platform identifies it as Zed's main process.
    pub(super) zed: bool,
}

/// The proxy that connected, its parent and its grandparent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Chain {
    pub(super) proxy: Process,
    pub(super) ssh: Process,
    pub(super) zed: Process,
}

/// A process, by pid and start, so a later process reusing the pid is a
/// different one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProcessKey {
    pub(super) pid: u32,
    pub(super) started: Option<u64>,
}

/// Why a connection was turned away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The chain behind the connection is not Tcode's proxy under Zed's ssh.
    NotZed,
    /// A Zed, but not the one Tcode opened this machine in.
    NotGranted,
}

/// The Zed behind `chain`: the proxy is this Tcode executable, its parent
/// is `ssh`, a ControlMaster invocation where `master_only`, and ssh's
/// parent is Zed's main process itself.
pub(super) fn zed_behind(
    chain: &Chain,
    own_exe: &Path,
    master_only: bool,
) -> Result<ProcessKey, Refusal> {
    if chain.proxy.exe.as_deref() != Some(own_exe) {
        return Err(Refusal::NotZed);
    }
    let is_ssh = chain
        .ssh
        .exe
        .as_deref()
        .and_then(Path::file_stem)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("ssh"));
    if !is_ssh || (master_only && !control_master(chain.ssh.args.as_deref())) || !chain.zed.zed {
        return Err(Refusal::NotZed);
    }
    Ok(ProcessKey {
        pid: chain.zed.pid,
        started: chain.zed.started,
    })
}

/// Whether ssh's arguments make it a ControlMaster, as Zed starts its
/// connection: `-o ControlMaster=yes` among the options, which end at the
/// destination as ssh's own parsing ends there.
fn control_master(args: Option<&[String]>) -> bool {
    /// ssh's options that take a value (ssh(1)).
    const WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let Some(args) = args else {
        return false;
    };
    let mut words = args.iter().skip(1);
    while let Some(word) = words.next() {
        if word == "--" {
            return false;
        }
        let Some(cluster) = word.strip_prefix('-').filter(|cluster| !cluster.is_empty()) else {
            return false;
        };
        for (index, flag) in cluster.char_indices() {
            if !WITH_VALUE.contains(flag) {
                continue;
            }
            let inline = &cluster[index + flag.len_utf8()..];
            let value = if inline.is_empty() {
                words.next().map(String::as_str)
            } else {
                Some(inline)
            };
            if flag == 'o'
                && value
                    .and_then(|option| option.split_once(['=', ' ']))
                    .is_some_and(|(key, value)| {
                        key.eq_ignore_ascii_case("ControlMaster")
                            && value.trim().eq_ignore_ascii_case("yes")
                    })
            {
                return true;
            }
            break;
        }
    }
    false
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
    pub(super) fn open(&mut self, machine: &str, now: Instant, alive: impl Fn(ProcessKey) -> bool) {
        if let Some(Grant::Bound(zed)) = self.machines.get(machine)
            && alive(*zed)
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
        alive: impl Fn(ProcessKey) -> bool,
    ) -> Result<(), Refusal> {
        match self.machines.get(machine).copied() {
            Some(Grant::Bound(bound)) if bound == zed => Ok(()),
            Some(Grant::Bound(bound)) if alive(bound) => Err(Refusal::NotGranted),
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

    const TCODE: &str = "/Applications/Tcode.app/Contents/MacOS/tcode";
    const ZED: &str = "/Applications/Zed.app/Contents/MacOS/zed";

    fn process(pid: u32, exe: &str, args: &[&str], zed: bool) -> Process {
        Process {
            pid,
            exe: Some(PathBuf::from(exe)),
            args: Some(args.iter().map(|arg| (*arg).to_owned()).collect()),
            started: Some(u64::from(pid) * 10),
            zed,
        }
    }

    fn chain(proxy: Process, ssh: Process, zed: Process) -> Chain {
        Chain { proxy, ssh, zed }
    }

    #[test]
    fn only_zeds_own_master_ssh_reaches_a_machine_and_only_the_zed_tcode_opened() {
        let tcode = Path::new(TCODE);
        let proxy = process(102, TCODE, &[TCODE, "ssh-proxy", "m"], false);
        let zed = process(100, ZED, &[ZED], true);
        // As Zed starts its connection (crates/remote/src/transport/ssh.rs,
        // MasterProcess::new).
        let master = process(
            101,
            "/usr/bin/ssh",
            &[
                "ssh",
                "-F",
                "/data/ssh/config",
                "-N",
                "-o",
                "ControlPersist=no",
                "-o",
                "ControlMaster=yes",
                "-o",
                "ControlPath=/tmp/zed-ssh-session/ssh.sock",
                "--",
                "tcode-m",
            ],
            false,
        );
        let own = chain(proxy.clone(), master.clone(), zed.clone());
        assert_eq!(zed_behind(&own, tcode, true).map(|key| key.pid), Ok(100));

        let typed = process(
            111,
            "/usr/bin/ssh",
            &[
                "ssh",
                "-F",
                "/data/ssh/config",
                "tcode-m",
                "-oControlMaster=yes",
            ],
            false,
        );
        let refused = [
            // `exec ssh tcode-m` in Zed's terminal: Zed's child, no master.
            chain(proxy.clone(), typed.clone(), zed.clone()),
            // Something else posing as the proxy.
            chain(
                process(120, "/tmp/tcode", &["/tmp/tcode"], false),
                master.clone(),
                zed.clone(),
            ),
            // A master started by anything but Zed.
            chain(
                proxy.clone(),
                master.clone(),
                process(130, "/tmp/Zed.app/Contents/MacOS/zed", &[], false),
            ),
            // Not ssh.
            chain(
                proxy.clone(),
                process(140, "/bin/zsh", &["zsh"], false),
                zed.clone(),
            ),
        ];
        for chain in &refused {
            assert_eq!(
                zed_behind(chain, tcode, true),
                Err(Refusal::NotZed),
                "{chain:?}"
            );
        }
        assert_eq!(
            zed_behind(&chain(proxy.clone(), typed, zed.clone()), tcode, false).map(|key| key.pid),
            Ok(100),
            "where Zed has no master, ssh started directly by Zed is all there is to check"
        );

        let first = zed_behind(&own, tcode, true).unwrap();
        let second = ProcessKey {
            pid: 200,
            started: Some(2000),
        };
        let running = |key: ProcessKey| key == first || key == second;
        let mut grants = Grants::default();
        let now = Instant::now();
        assert_eq!(
            grants.admit("m", first, now, running),
            Err(Refusal::NotGranted),
            "no Open in Zed yet"
        );
        grants.open("m", now, running);
        assert_eq!(
            grants.admit("other", first, now, running),
            Err(Refusal::NotGranted),
            "the window is per machine"
        );
        assert_eq!(grants.admit("m", first, now, running), Ok(()));
        assert_eq!(
            grants.admit("m", second, now, running),
            Err(Refusal::NotGranted),
            "the first Zed took the machine"
        );
        grants.open("m", now, running);
        assert_eq!(
            grants.admit("m", second, now, running),
            Err(Refusal::NotGranted),
            "opening again keeps the running Zed bound"
        );
        assert_eq!(
            grants.admit("m", first, now + WINDOW * 10, running),
            Ok(()),
            "the bound Zed stays admitted after the window"
        );

        // The bound Zed exits; a late connection finds nothing, and the
        // next Open in Zed lets the other Zed take the machine.
        let only_second = |key: ProcessKey| key == second;
        let later = now + WINDOW * 2;
        assert_eq!(
            grants.admit("m", second, later, only_second),
            Err(Refusal::NotGranted)
        );
        grants.open("m", later, only_second);
        assert_eq!(grants.admit("m", second, later, only_second), Ok(()));
        grants.open("x", later, only_second);
        assert_eq!(
            grants.admit("x", second, later + WINDOW, only_second),
            Err(Refusal::NotGranted),
            "the window closes after a minute"
        );
    }
}
