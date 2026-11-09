//! A harness CLI's own account sign-in and sign-out (`claude auth login`,
//! `codex login --device-auth`, …), run for one profile's config directory.

use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use agent::{LaunchEnv, ProviderKind};
use futures_lite::{AsyncReadExt as _, AsyncWriteExt as _, future};
use smol::process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio};

use crate::provider_probe::provider_command;

/// What a running sign-in has reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginEvent {
    /// The sign-in page the CLI printed; reported once.
    Url(String),
    /// The one-time code the CLI printed for the user to enter on the
    /// sign-in page; reported once.
    DeviceCode(String),
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
    exit: future::Boxed<std::io::Result<ExitStatus>>,
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

/// Spawn the harness's sign-in with the profile's environment.
pub fn start_login(
    provider: ProviderKind,
    binary: &Path,
    launch_env: &LaunchEnv,
) -> std::io::Result<(LoginHandle, LoginRun)> {
    let env = launch_env.pairs(provider);
    let mut child = provider_command(&binary.to_string_lossy(), &env)
        .args(provider.harness().login_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let (sender, codes) = smol::channel::unbounded();
    let stdin = child.stdin.take();
    let run = LoginRun {
        exit: Box::pin(child.status()),
        stdin,
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
    Exited(std::io::Result<ExitStatus>),
}

/// How long output the CLI wrote before exiting is still read; a process it
/// started may hold its pipes open long after.
const DRAIN: Duration = Duration::from_secs(1);

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
            let (stdout, stderr, codes, exit) = (
                &mut self.stdout,
                &mut self.stderr,
                &self.codes,
                &mut self.exit,
            );
            let cancelled = self.cancelled;
            let step = future::or(
                future::or(
                    async move { Step::Stdout(read_chunk(stdout).await) },
                    async move { Step::Stderr(read_chunk(stderr).await) },
                ),
                future::or(
                    async move {
                        if cancelled {
                            future::pending().await
                        } else {
                            Step::Code(codes.recv().await.ok())
                        }
                    },
                    async move { Step::Exited(exit.await) },
                ),
            )
            .await;
            match step {
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
                Step::Exited(status) => {
                    self.drain().await;
                    self.finished = true;
                    let result = match status {
                        Ok(status) if status.success() => Ok(()),
                        Ok(status) => Err(self.failure(&status.to_string())),
                        Err(error) => Err(error.to_string()),
                    };
                    self.pending.push_back(LoginEvent::Finished(result));
                }
                output => self.take_output(output),
            }
        }
    }

    fn take_output(&mut self, step: Step) {
        match step {
            Step::Stdout(Some(chunk)) => self.pending.extend(self.output.feed(&chunk)),
            Step::Stdout(None) => self.stdout = None,
            Step::Stderr(Some(chunk)) => self.errors.extend_from_slice(&chunk),
            Step::Stderr(None) => self.stderr = None,
            Step::Code(_) | Step::Exited(_) => {}
        }
    }

    async fn drain(&mut self) {
        let mut deadline = smol::Timer::after(DRAIN);
        while self.stdout.is_some() || self.stderr.is_some() {
            let (stdout, stderr) = (&mut self.stdout, &mut self.stderr);
            let step = future::or(
                future::or(
                    async move { Some(Step::Stdout(read_chunk(stdout).await)) },
                    async move { Some(Step::Stderr(read_chunk(stderr).await)) },
                ),
                async {
                    (&mut deadline).await;
                    None
                },
            )
            .await;
            match step {
                Some(step) => self.take_output(step),
                None => break,
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

/// The sign-in's stdout, read as it arrives.
#[derive(Default)]
struct LoginOutput {
    text: Vec<u8>,
    url_seen: bool,
    code_seen: bool,
    prompts: usize,
}

impl LoginOutput {
    fn feed(&mut self, chunk: &[u8]) -> Vec<LoginEvent> {
        self.text.extend_from_slice(chunk);
        // Codex colors its output even into a pipe.
        let text = agent::strip_ansi(&String::from_utf8_lossy(&self.text));
        let mut events = Vec::new();
        let complete = text.rfind('\n').map_or("", |end| &text[..end]);
        if !self.url_seen
            && let Some(url) = complete
                .lines()
                .find_map(|line| line.find("https://").map(|start| &line[start..]))
                .and_then(|url| url.split_whitespace().next())
        {
            self.url_seen = true;
            events.push(LoginEvent::Url(url.to_string()));
        }
        if self.url_seen
            && !self.code_seen
            && let Some(code) = complete
                .lines()
                .map(str::trim)
                .find(|line| is_device_code(line))
        {
            self.code_seen = true;
            events.push(LoginEvent::DeviceCode(code.to_string()));
        }
        let prompts = text.matches(CODE_PROMPT).count();
        if prompts > self.prompts {
            self.prompts = prompts;
            events.push(LoginEvent::AwaitingCode);
        }
        events
    }
}

/// A device-authorization code on a line of its own: groups of uppercase
/// letters and digits joined by hyphens, as `3JP6-MXLMX`.
fn is_device_code(line: &str) -> bool {
    let groups: Vec<&str> = line.split('-').collect();
    groups.len() >= 2
        && groups.iter().all(|group| {
            (3..=8).contains(&group.len())
                && group
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
        })
}

/// The last non-empty line, without the code prompt that precedes it on the
/// same line.
fn last_line(output: &[u8]) -> Option<String> {
    let text = agent::strip_ansi(&String::from_utf8_lossy(output));
    text.lines()
        .map(|line| match line.find(CODE_PROMPT) {
            Some(start) => line[start..].split_once('>').map_or("", |(_, rest)| rest),
            None => line,
        })
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

/// Sign the profile out through the harness's CLI.
pub async fn logout(
    provider: ProviderKind,
    binary: &Path,
    launch_env: &LaunchEnv,
) -> Result<(), String> {
    let env = launch_env.pairs(provider);
    let output = provider_command(&binary.to_string_lossy(), &env)
        .args(provider.harness().logout_args)
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

    /// What codex-cli 0.166.0 prints on stdout for `codex login --device-auth`
    /// into a pipe, colors included.
    const CODEX_OUTPUT: &str = "\nWelcome to Codex [v\u{1b}[90m0.166.0\u{1b}[0m]\n\u{1b}[90mOpenAI's command-line coding agent\u{1b}[0m\n\nFollow these steps to sign in with ChatGPT using device code authorization:\n\n1. Open this link in your browser and sign in to your account\n   \u{1b}[94mhttps://auth.openai.com/codex/device\u{1b}[0m\n\n2. Enter this one-time code \u{1b}[90m(expires in 15 minutes)\u{1b}[0m\n   \u{1b}[94m3JP6-MXLMX\u{1b}[0m\n\n\u{1b}[90mContinue only if you started this login in Codex. If a website or another person gave you this code, cancel.\u{1b}[0m\n";

    #[test]
    fn device_login_output_yields_the_page_and_the_code_to_enter_there() {
        let bytes = CODEX_OUTPUT.as_bytes();
        let code = CODEX_OUTPUT.find("3JP6").unwrap();
        for splits in [vec![], vec![code + 2], vec![40, code + 6]] {
            let mut output = LoginOutput::default();
            let mut events = Vec::new();
            let mut start = 0;
            for end in splits.into_iter().chain([bytes.len()]) {
                events.extend(output.feed(&bytes[start..end]));
                start = end;
            }
            assert_eq!(
                events,
                vec![
                    LoginEvent::Url("https://auth.openai.com/codex/device".into()),
                    LoginEvent::DeviceCode("3JP6-MXLMX".into()),
                ]
            );
        }
    }

    /// A browser the CLI launched keeps its stdout open; the sign-in still
    /// finishes when the CLI exits.
    #[cfg(unix)]
    #[test]
    fn login_finishes_when_the_cli_exits_though_a_process_it_started_holds_its_output() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("tcode-login-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("claude");
        std::fs::write(
            &binary,
            "#!/bin/sh\nsleep 30 &\nread code\necho 'Login successful.'\n",
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = std::time::Instant::now();
        let finished = smol::block_on(async {
            let (handle, mut run) =
                start_login(ProviderKind::ClaudeCode, &binary, &LaunchEnv::default()).unwrap();
            handle.submit_code("code".into());
            let mut finished = None;
            while let Some(event) = run.next().await {
                if let LoginEvent::Finished(result) = event {
                    finished = Some(result);
                }
            }
            finished
        });
        assert_eq!(finished, Some(Ok(())));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(dir);
    }
}
