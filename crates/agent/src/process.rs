//! Child-process constructors that never flash a console window on Windows.
//!
//! Provider CLIs are spawned from a GUI app; a plain `Command` on Windows
//! allocates a console for the child, which pops a black box on screen.
//! `CREATE_NO_WINDOW` suppresses it. No-ops elsewhere.

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A blocking `std::process::Command` with the console suppressed.
pub(crate) fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// An async (`smol`) command with the console suppressed. `async_process`
/// exposes no `creation_flags`, so the flag rides in through the `From` impl.
pub(crate) fn async_command<S: AsRef<std::ffi::OsStr>>(program: S) -> smol::process::Command {
    smol::process::Command::from(command(program))
}
