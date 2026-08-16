//! Child-process constructors that never flash a console window on Windows.
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

pub(crate) async fn unblock<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    smol::unblock(f).await
}

pub(crate) async fn probe_version(
    binary: &std::path::Path,
    launch_env: &crate::LaunchEnv,
    provider: crate::ProviderKind,
) -> Option<(u32, u32, u32)> {
    let mut command = async_command(binary);
    command.arg("--version");
    if provider == crate::ProviderKind::ClaudeCode {
        command
            .env_remove("CLAUDECODE")
            .env_remove("CLAUDE_CODE_ENTRYPOINT");
    }
    for (key, value) in launch_env.pairs(provider) {
        command.env(key, value);
    }
    let output = command.output().await.ok()?;
    parse_semver(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) fn parse_semver(text: &str) -> Option<(u32, u32, u32)> {
    let token = text.split_whitespace().find(|token| token.contains('.'))?;
    let mut parts = token.trim_start_matches('v').split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts
            .next()
            .and_then(|part| part.split(['-', '+']).next())?
            .parse()
            .ok()?,
    ))
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
