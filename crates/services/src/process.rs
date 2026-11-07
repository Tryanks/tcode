//! Child-process helpers.
//!
//! Every process tcode spawns (git, the provider CLIs, npm, editors, …) must go
//! through these constructors, for four Windows reasons:
//!
//! 1. A plain `Command` pops a console window for the child, which is jarring in
//!    a GUI app. `CREATE_NO_WINDOW` suppresses it.
//! 2. `CreateProcess` only ever appends `.exe` to a bare program name, so the
//!    `npm` / `pnpm` / `bun` (and `claude`) **`.cmd` shims** are invisible to it.
//!    [`resolve_program`] does a `PATHEXT`-aware PATH search first (shared with
//!    the agent crate's provider-binary resolution).
//! 3. A `.cmd` runs through `cmd.exe`, which cannot take an argument that spans
//!    lines; a package manager's shim is started as the program it runs.
//! 4. A child inherits every inheritable handle of tcode's, tcode's own stdout
//!    and stderr included, so a child that outlives tcode keeps whoever reads
//!    tcode's output waiting.
//!
//! [`agent::command`] owns 1, 3 and 4. On other platforms all of them are
//! no-ops / passthroughs — `exec` resolves PATH.

/// Resolve a bare program name to an absolute path on Windows (`npm` →
/// `C:\…\npm.cmd`), so `CreateProcess` — which only knows about `.exe` — can
/// actually find `.cmd`/`.bat` shims. Anything with a path separator, and every
/// non-Windows target, is passed through untouched.
pub fn resolve_program<S: AsRef<std::ffi::OsStr>>(program: S) -> std::ffi::OsString {
    let program = program.as_ref();
    if !cfg!(windows) {
        return program.to_os_string();
    }
    let Some(name) = program.to_str() else {
        return program.to_os_string();
    };
    if name.contains(['/', '\\']) {
        return program.to_os_string();
    }
    agent::find_on_path(name)
        .map(std::ffi::OsString::from)
        // No match: hand the bare name to the OS and let it report the failure.
        .unwrap_or_else(|| program.to_os_string())
}

/// A `std::process::Command` with its program resolved through
/// [`resolve_program`] and started as [`agent::command`] starts it.
pub fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    agent::command(resolve_program(program))
}

/// The same, for the async (`smol`) command type used by long-lived children.
///
/// `async_process::Command` has no `creation_flags` of its own, but it converts
/// from a `std::process::Command` — so the flag is set before the conversion.
pub fn async_command<S: AsRef<std::ffi::OsStr>>(program: S) -> smol::process::Command {
    smol::process::Command::from(command(program))
}

/// Raise this process's soft limit on open files to its hard limit; children
/// inherit it.
///
/// A host holds several descriptors per provider session (stdio pipes and the
/// loopback MCP connections), and a Finder/Dock launch inherits launchd's soft
/// limit of 256, which a few dozen sessions exhaust; every later open in the
/// process then fails with `EMFILE`.
#[cfg(unix)]
pub fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable `rlimit`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        log::warn!(
            "could not read the open-file limit: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    // macOS rejects a soft limit above OPEN_MAX (<sys/syslimits.h>) even when
    // the hard limit is unlimited; setrlimit(2) prescribes min(OPEN_MAX, max).
    #[cfg(target_vendor = "apple")]
    let target = limit.rlim_max.min(10240);
    #[cfg(not(target_vendor = "apple"))]
    let target = limit.rlim_max;
    if target <= limit.rlim_cur {
        return;
    }
    let raised = libc::rlimit {
        rlim_cur: target,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: `raised` is a valid `rlimit` that does not exceed the hard limit.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
        log::warn!(
            "could not raise the open-file limit from {} to {target}: {}",
            limit.rlim_cur,
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
pub fn raise_open_file_limit() {}
