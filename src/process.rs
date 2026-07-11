//! Child-process helpers.
//!
//! Every process tcode spawns (git, the provider CLIs, npm, editors, …) must go
//! through these constructors, for two Windows reasons:
//!
//! 1. A plain `Command` pops a console window for the child, which is jarring in
//!    a GUI app. `CREATE_NO_WINDOW` suppresses it.
//! 2. `CreateProcess` only ever appends `.exe` to a bare program name, so the
//!    `npm` / `pnpm` / `bun` (and `claude`) **`.cmd` shims** are invisible to it.
//!    [`resolve_program`] does a `PATHEXT`-aware PATH search first (shared with
//!    the agent crate's provider-binary resolution).
//!
//! On other platforms both are no-ops / passthroughs — `exec` resolves PATH.

/// `CREATE_NO_WINDOW`: run the child without allocating a console.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Resolve a bare program name to an absolute path on Windows (`npm` →
/// `C:\…\npm.cmd`), so `CreateProcess` — which only knows about `.exe` — can
/// actually find `.cmd`/`.bat` shims. Anything with a path separator, and every
/// non-Windows target, is passed through untouched.
///
/// Spawning a `.cmd`/`.bat` through `std::process::Command` is supported by Rust
/// (it applies the post-CVE-2024-24576 batch-file argument escaping), so no
/// `cmd /c` wrapper is needed once the path resolves.
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

/// A `std::process::Command` that never flashes a console window, with its
/// program resolved through [`resolve_program`].
pub fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    let mut cmd = std::process::Command::new(resolve_program(program));
    hide_console(&mut cmd);
    cmd
}

/// Suppress the child's console window (no-op off Windows).
#[cfg(windows)]
pub fn hide_console(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt as _;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn hide_console(_cmd: &mut std::process::Command) {}

/// The same, for the async (`smol`) command type used by long-lived children.
///
/// `async_process::Command` has no `creation_flags` of its own, but it converts
/// from a `std::process::Command` — so the flag is set before the conversion.
pub fn async_command<S: AsRef<std::ffi::OsStr>>(program: S) -> smol::process::Command {
    smol::process::Command::from(command(program))
}

#[cfg(test)]
mod tests {
    use super::resolve_program;

    /// Off Windows, resolution is a passthrough: `exec` searches PATH itself and
    /// an absolute path would defeat a caller that means "whatever is on PATH".
    #[cfg(not(windows))]
    #[test]
    fn resolve_program_is_a_passthrough_off_windows() {
        assert_eq!(resolve_program("npm"), std::ffi::OsString::from("npm"));
        assert_eq!(
            resolve_program("/usr/bin/git"),
            std::ffi::OsString::from("/usr/bin/git")
        );
    }

    /// A path (either separator) is never PATH-searched, on any platform.
    #[test]
    fn resolve_program_passes_paths_through() {
        assert_eq!(
            resolve_program(r"C:\tools\npm.cmd"),
            std::ffi::OsString::from(r"C:\tools\npm.cmd")
        );
    }

    /// Guard: every child process must go through this module, or Windows users
    /// get a console window flashed at them. If this fails, use
    /// `crate::process::command` / `async_command` instead of `Command::new`.
    #[test]
    fn no_direct_command_new_outside_this_module() {
        let mut offenders = Vec::new();
        let roots = ["src", "crates/agent/src", "crates/preview-mcp/src"];
        for root in roots {
            visit(std::path::Path::new(root), &mut offenders);
        }
        assert!(
            offenders.is_empty(),
            "spawn sites bypassing crate::process (Windows would pop a console): {offenders:#?}"
        );
    }

    fn visit(dir: &std::path::Path, offenders: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, offenders);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            // The helpers themselves are the only legitimate callers.
            if path.file_name().is_some_and(|name| name == "process.rs") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (idx, line) in source.lines().enumerate() {
                if line.contains("Command::new(") && !line.trim_start().starts_with("//") {
                    offenders.push(format!("{}:{}", path.display(), idx + 1));
                }
            }
        }
    }
}
