//! Child-process helpers.
//!
//! Every process tcode spawns (git, the provider CLIs, editors, …) must go
//! through these constructors: on Windows a plain `Command` pops a console
//! window for the child, which is jarring in a GUI app. `CREATE_NO_WINDOW`
//! suppresses it. On other platforms these are thin passthroughs.

/// `CREATE_NO_WINDOW`: run the child without allocating a console.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A `std::process::Command` that never flashes a console window.
pub fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
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
