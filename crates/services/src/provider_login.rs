//! Claude Code's own account sign-in and sign-out (`claude auth login` /
//! `claude auth logout`), run for one profile's config directory.

use std::path::Path;

use agent::{LaunchEnv, ProviderKind};
use futures_lite::{AsyncReadExt as _, AsyncWriteExt as _, future};
use smol::process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio};

use crate::provider_probe::provider_command;

/// What a running `claude auth login` has reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginEvent {
    /// The sign-in page the CLI printed; reported once.
    Url(String),
    /// The CLI prompts for the code the sign-in page shows.
    AwaitingCode,
    /// The CLI exited; an error carries its last words or exit status.
    Finished(Result<(), String>),
}

/// Controls a running login. Dropping it, like [`LoginHandle::cancel`], kills
/// the CLI.
pub struct LoginHandle {
    codes: smol::channel::Sender<String>,
}

impl LoginHandle {
    pub fn submit_code(&self, code: String) {
        let _ = self.codes.try_send(code);
    }

    pub fn cancel(&self) {
        self.codes.close();
    }
}

/// The CLI's side of a login; [`LoginRun::next`] drives it.
pub struct LoginRun {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    codes: smol::channel::Receiver<String>,
    cancelled: bool,
    output: LoginOutput,
    errors: Vec<u8>,
    pending: std::collections::VecDeque<LoginEvent>,
    finished: bool,
}

/// Spawn `<binary> auth login` with the profile's environment.
pub fn start_login(
    binary: &Path,
    launch_env: &LaunchEnv,
) -> std::io::Result<(LoginHandle, LoginRun)> {
    let env = launch_env.pairs(ProviderKind::ClaudeCode);
    let mut child = provider_command(&binary.to_string_lossy(), &env)
        .args(["auth", "login"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let (sender, codes) = smol::channel::unbounded();
    let run = LoginRun {
        stdin: child.stdin.take(),
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
        child,
        codes,
        cancelled: false,
        output: LoginOutput::default(),
        errors: Vec::new(),
        pending: Default::default(),
        finished: false,
    };
    Ok((LoginHandle { codes: sender }, run))
}

enum Step {
    Stdout(Option<Vec<u8>>),
    Stderr(Option<Vec<u8>>),
    Code(Option<String>),
}

impl LoginRun {
    /// The next event; `None` after [`LoginEvent::Finished`].
    pub async fn next(&mut self) -> Option<LoginEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.finished {
                return None;
            }
            if self.stdout.is_none() && self.stderr.is_none() {
                self.finished = true;
                let status = self.child.status().await;
                let result = match status {
                    Ok(status) if status.success() => Ok(()),
                    Ok(status) => Err(self.failure(&status.to_string())),
                    Err(error) => Err(error.to_string()),
                };
                return Some(LoginEvent::Finished(result));
            }
            let (stdout, stderr, codes) = (&mut self.stdout, &mut self.stderr, &self.codes);
            let cancelled = self.cancelled;
            let step = future::or(
                future::or(
                    async move { Step::Stdout(read_chunk(stdout).await) },
                    async move { Step::Stderr(read_chunk(stderr).await) },
                ),
                async move {
                    if cancelled {
                        future::pending().await
                    } else {
                        Step::Code(codes.recv().await.ok())
                    }
                },
            )
            .await;
            match step {
                Step::Stdout(Some(chunk)) => self.pending.extend(self.output.feed(&chunk)),
                Step::Stdout(None) => self.stdout = None,
                Step::Stderr(Some(chunk)) => self.errors.extend_from_slice(&chunk),
                Step::Stderr(None) => self.stderr = None,
                Step::Code(Some(code)) => {
                    if let Some(stdin) = &mut self.stdin {
                        let line = format!("{code}\n");
                        if stdin.write_all(line.as_bytes()).await.is_err()
                            || stdin.flush().await.is_err()
                        {
                            self.stdin = None;
                        }
                    }
                }
                Step::Code(None) => {
                    self.cancelled = true;
                    let _ = self.child.kill();
                }
            }
        }
    }

    fn failure(&self, status: &str) -> String {
        last_line(&self.errors)
            .or_else(|| last_line(&self.output.text))
            .unwrap_or_else(|| status.to_string())
    }
}

/// The next chunk of a stream, `None` at its end; a closed stream never yields.
async fn read_chunk(stream: &mut Option<impl futures_lite::AsyncRead + Unpin>) -> Option<Vec<u8>> {
    let Some(stream) = stream else {
        return future::pending().await;
    };
    let mut buffer = [0u8; 4096];
    match stream.read(&mut buffer).await {
        Ok(0) | Err(_) => None,
        Ok(read) => Some(buffer[..read].to_vec()),
    }
}

/// The prompt has no trailing newline, so it is matched in the accumulated
/// output rather than per line.
const CODE_PROMPT: &str = "Paste code";

/// `claude auth login`'s stdout, read as it arrives.
#[derive(Default)]
struct LoginOutput {
    text: Vec<u8>,
    url_seen: bool,
    prompts: usize,
}

impl LoginOutput {
    fn feed(&mut self, chunk: &[u8]) -> Vec<LoginEvent> {
        self.text.extend_from_slice(chunk);
        let text = String::from_utf8_lossy(&self.text);
        let mut events = Vec::new();
        if !self.url_seen {
            let complete = text.rfind('\n').map_or("", |end| &text[..end]);
            if let Some(url) = complete
                .lines()
                .find_map(|line| line.find("https://").map(|start| &line[start..]))
                .and_then(|url| url.split_whitespace().next())
            {
                self.url_seen = true;
                events.push(LoginEvent::Url(url.to_string()));
            }
        }
        let prompts = text.matches(CODE_PROMPT).count();
        if prompts > self.prompts {
            self.prompts = prompts;
            events.push(LoginEvent::AwaitingCode);
        }
        events
    }
}

/// The last non-empty line, without the code prompt that precedes it on the
/// same line.
fn last_line(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    text.lines()
        .map(|line| match line.find(CODE_PROMPT) {
            Some(start) => line[start..].split_once('>').map_or("", |(_, rest)| rest),
            None => line,
        })
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

/// Run `<binary> auth logout` with the profile's environment.
pub async fn logout(binary: &Path, launch_env: &LaunchEnv) -> Result<(), String> {
    let env = launch_env.pairs(ProviderKind::ClaudeCode);
    let output = provider_command(&binary.to_string_lossy(), &env)
        .args(["auth", "logout"])
        .output()
        .await
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    Err(last_line(&output.stderr)
        .or_else(|| last_line(&output.stdout))
        .unwrap_or_else(|| output.status.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What claude 2.1.314 prints on stdout without a terminal, its query
    /// values shortened.
    const OUTPUT: &str = "Opening browser to sign in…\nIf the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true&client_id=CLIENT&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=SCOPE&code_challenge=CHALLENGE&code_challenge_method=S256&state=STATE\nPaste code here if prompted > ";

    #[test]
    fn login_output_yields_the_sign_in_page_then_the_code_prompt_across_any_chunking() {
        let url = "https://claude.com/cai/oauth/authorize?code=true&client_id=CLIENT&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=SCOPE&code_challenge=CHALLENGE&code_challenge_method=S256&state=STATE";
        let expected = vec![LoginEvent::Url(url.into()), LoginEvent::AwaitingCode];
        let bytes = OUTPUT.as_bytes();
        let prompt = OUTPUT.find("Paste code").unwrap();
        let url_start = OUTPUT.find("https://").unwrap();
        // Splits inside "…" (multi-byte), inside the URL and inside "Paste code".
        for splits in [
            vec![],
            vec![27],
            vec![url_start + 40],
            vec![prompt + 3],
            vec![url_start + 10, prompt + 6],
        ] {
            let mut output = LoginOutput::default();
            let mut events = Vec::new();
            let mut start = 0;
            for end in splits.into_iter().chain([bytes.len()]) {
                events.extend(output.feed(&bytes[start..end]));
                start = end;
            }
            assert_eq!(events, expected);
        }
    }
}
