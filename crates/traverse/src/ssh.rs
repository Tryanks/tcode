//! The SSH server a machine runs on each admitted `ssh` stream, so an
//! editor's own remote development reaches the machine over the device's
//! connection. It listens on no port: the device is authenticated by iroh
//! and hello before the stream exists, so SSH authentication is `none`.
//!
//! A command runs through the machine user's shell as OpenSSH runs it, in
//! the home directory with the client's `env` requests applied: `$SHELL -c`
//! on Unix, and on Windows the shell Tcode's terminals use (`%COMSPEC%`,
//! cmd.exe, which is also Win32-OpenSSH's default) with `/c`. A session that
//! asked for a pseudoterminal runs in one. The `sftp` subsystem is served in
//! process. Port, X11 and agent forwarding are refused. Closing a channel
//! ends the process it started.
mod sftp;

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use iroh::endpoint::SendStream;
use russh::{
    Channel, ChannelId, ChannelMsg, ChannelReadHalf, ChannelWriteHalf, MethodKind, Sig,
    keys::PrivateKey,
    server::{self, Auth, ChannelOpenHandle, Msg, Session},
};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};

use crate::wire::{self, HostLine, LineReader};

/// The machine's SSH host key, beside its iroh identity in the data dir.
const HOST_KEY_FILE: &str = "ssh_host_ed25519_key";

pub(crate) struct Server {
    config: Arc<server::Config>,
}

impl Server {
    /// The server with this machine's host key, created on first use.
    pub(crate) fn load(data_dir: &Path) -> io::Result<Self> {
        let key = host_key(&data_dir.join(HOST_KEY_FILE))?;
        Ok(Self {
            config: Arc::new(server::Config {
                server_id: russh::SshId::Standard("SSH-2.0-Tcode".into()),
                methods: (&[MethodKind::None][..]).into(),
                keys: vec![key],
                // An editor leaves its connection quiet for long stretches;
                // the client's keepalives and the iroh connection end a
                // dead one.
                inactivity_timeout: None,
                ..Default::default()
            }),
        })
    }

    /// Take the stream: say so, then speak SSH on it until either side ends.
    pub(crate) async fn serve(&self, mut send: SendStream, reader: LineReader) -> io::Result<()> {
        wire::write_line(&mut send, &HostLine::Connected).await?;
        let session = server::run_stream(
            self.config.clone(),
            tokio::io::join(reader, send),
            Connection,
        )
        .await
        .map_err(io::Error::other)?;
        session.await.map_err(io::Error::other)
    }
}

fn host_key(path: &Path) -> io::Result<PrivateKey> {
    match std::fs::read(path) {
        Ok(bytes) => PrivateKey::from_openssh(bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut seed = [0; 32];
            getrandom::fill(&mut seed).map_err(io::Error::other)?;
            let key = PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
                &seed,
            ));
            let encoded = key
                .to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .map_err(io::Error::other)?;
            crate::identity::write_private(path, encoded.as_bytes())?;
            Ok(key)
        }
        Err(error) => Err(error),
    }
}

/// One SSH connection. Each session channel runs on a task of its own; the
/// requests the client wants answered are answered here.
struct Connection;

impl server::Handler for Connection {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        tokio::spawn(run_channel(channel, session.handle()));
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        _variable_name: &str,
        _variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp" {
            session.channel_success(channel)
        } else {
            session.channel_failure(channel)
        }
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single_connection: bool,
        _x11_auth_protocol: &str,
        _x11_auth_cookie: &str,
        _x11_screen_number: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)
    }
}

/// What a session channel was asked to run.
enum Start {
    Command(String),
    Shell,
    Sftp,
}

#[derive(Default)]
struct Setup {
    environment: Vec<(String, String)>,
    pty: Option<PtySize>,
    /// Input that arrived before the program started.
    early: Vec<u8>,
    eof: bool,
}

#[derive(Clone, Copy)]
struct PtySize {
    cols: u16,
    rows: u16,
    width: u16,
    height: u16,
}

impl PtySize {
    fn new(cols: u32, rows: u32, width: u32, height: u32) -> Self {
        let clamp = |value: u32| u16::try_from(value).unwrap_or(u16::MAX);
        Self {
            cols: clamp(cols).max(1),
            rows: clamp(rows).max(1),
            width: clamp(width),
            height: clamp(height),
        }
    }
}

/// How a program ended, as the client is told.
enum Exit {
    Status(u32),
    // Only Unix processes end by a signal.
    #[cfg_attr(not(unix), allow(dead_code))]
    Signal {
        signal: Sig,
        core_dumped: bool,
    },
    /// Ended without a status the platform reports.
    Unknown,
}

async fn run_channel(channel: Channel<Msg>, handle: server::Handle) {
    let id = channel.id();
    let (mut input, output) = channel.split();
    let mut setup = Setup::default();
    let start = loop {
        let Some(message) = input.wait().await else {
            return;
        };
        match message {
            ChannelMsg::RequestPty {
                term,
                col_width,
                row_height,
                pix_width,
                pix_height,
                ..
            } => {
                setup.pty = Some(PtySize::new(col_width, row_height, pix_width, pix_height));
                setup.environment.push(("TERM".into(), term));
            }
            ChannelMsg::WindowChange {
                col_width,
                row_height,
                pix_width,
                pix_height,
            } => {
                if setup.pty.is_some() {
                    setup.pty = Some(PtySize::new(col_width, row_height, pix_width, pix_height));
                }
            }
            ChannelMsg::SetEnv {
                variable_name,
                variable_value,
                ..
            } => setup.environment.push((variable_name, variable_value)),
            ChannelMsg::Exec { command, .. } => {
                break Start::Command(String::from_utf8_lossy(&command).into_owned());
            }
            ChannelMsg::RequestShell { .. } => break Start::Shell,
            ChannelMsg::RequestSubsystem { name, .. } if name == "sftp" => break Start::Sftp,
            ChannelMsg::Data { data } => setup.early.extend_from_slice(&data),
            ChannelMsg::Eof => setup.eof = true,
            ChannelMsg::Close => return,
            _ => {}
        }
    };
    let home = std::env::home_dir().unwrap_or_else(std::env::temp_dir);
    let exit = match start {
        Start::Sftp => serve_sftp(&mut input, &output, setup, home).await,
        start => match setup.pty {
            Some(size) => run_pty(&mut input, &output, start, setup, size, &home).await,
            None => run_pipes(&mut input, &output, start, setup, &home).await,
        },
    };
    // `None`: the client closed the channel, which russh already answered.
    let Some(exit) = exit else {
        return;
    };
    match exit {
        Exit::Status(status) => {
            let _ = output.exit_status(status).await;
        }
        Exit::Signal {
            signal,
            core_dumped,
        } => {
            let _ = handle
                .exit_signal_request(id, signal, core_dumped, String::new(), String::new())
                .await;
        }
        Exit::Unknown => {}
    }
    let _ = output.eof().await;
    let _ = output.close().await;
}

/// The user's shell running `command` the way OpenSSH's server runs it.
fn shell_command(command: &str) -> std::process::Command {
    let (shell, _) = term::user_shell();
    let mut process = tcode_services::process::command(&shell);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // cmd.exe parses its own command line, so the client's command goes
        // through verbatim, as Win32-OpenSSH passes it.
        let flag = if is_cmd(&shell) { "/c" } else { "-c" };
        process.raw_arg(format!("{flag} {command}"));
    }
    #[cfg(not(windows))]
    process.arg("-c").arg(command);
    process
}

#[cfg(windows)]
fn is_cmd(shell: &str) -> bool {
    Path::new(shell)
        .file_stem()
        .is_some_and(|stem| stem.eq_ignore_ascii_case("cmd"))
}

/// The program and arguments a pseudoterminal session starts.
fn pty_program(start: &Start) -> (String, Vec<String>) {
    match start {
        Start::Command(command) => {
            let (shell, _) = term::user_shell();
            #[cfg(windows)]
            let flag = if is_cmd(&shell) { "/c" } else { "-c" };
            #[cfg(not(windows))]
            let flag = "-c";
            (shell, vec![flag.to_owned(), command.clone()])
        }
        Start::Shell | Start::Sftp => term::user_shell(),
    }
}

async fn run_pipes(
    input: &mut ChannelReadHalf,
    output: &ChannelWriteHalf<Msg>,
    start: Start,
    setup: Setup,
    home: &Path,
) -> Option<Exit> {
    let mut command = match &start {
        Start::Command(command) => shell_command(command),
        Start::Shell | Start::Sftp => {
            let (shell, args) = term::user_shell();
            let mut process = tcode_services::process::command(shell);
            process.args(args);
            process
        }
    };
    command
        .current_dir(home)
        .envs(setup.environment)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("tcode: could not start the shell: {error}\r\n");
            let _ = output.extended_data_bytes(1, message.into_bytes()).await;
            return Some(Exit::Status(127));
        }
    };
    #[cfg_attr(not(unix), allow(unused_variables))]
    let pid = child.id();
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    if let Some(writer) = &mut stdin
        && !setup.early.is_empty()
        && writer.write_all(&setup.early).await.is_err()
    {
        stdin = None;
    }
    if setup.eof {
        stdin = None;
    }
    let finished = async {
        tokio::join!(
            copy_out(stdout, output.make_writer()),
            copy_out(stderr, output.make_writer_ext(Some(1))),
        );
        child.wait().await
    };
    let feed = async {
        loop {
            match input.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    if let Some(writer) = &mut stdin
                        && writer.write_all(&data).await.is_err()
                    {
                        stdin = None;
                    }
                }
                Some(ChannelMsg::Eof) => stdin = None,
                Some(ChannelMsg::Signal { signal }) => {
                    #[cfg(unix)]
                    if let (Some(pid), Some(number)) = (pid, signal_number(&signal)) {
                        // SAFETY: kill(2) on our own child's pid.
                        unsafe { libc::kill(pid as libc::pid_t, number) };
                    }
                    #[cfg(not(unix))]
                    let _ = signal;
                }
                Some(ChannelMsg::Close) | None => return,
                Some(_) => {}
            }
        }
    };
    tokio::select! {
        status = finished => Some(match status {
            Ok(status) => exit_of(status),
            Err(_) => Exit::Unknown,
        }),
        () = feed => None,
    }
}

async fn copy_out(
    from: Option<impl tokio::io::AsyncRead + Unpin>,
    mut to: impl AsyncWrite + Unpin,
) {
    if let Some(mut from) = from {
        let _ = tokio::io::copy(&mut from, &mut to).await;
    }
}

fn exit_of(status: std::process::ExitStatus) -> Exit {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(number) = status.signal() {
            return Exit::Signal {
                signal: signal_of(number),
                core_dumped: status.core_dumped(),
            };
        }
    }
    match status.code() {
        Some(code) => Exit::Status(code as u32),
        None => Exit::Unknown,
    }
}

#[cfg(unix)]
fn signal_of(number: libc::c_int) -> Sig {
    match number {
        libc::SIGABRT => Sig::ABRT,
        libc::SIGALRM => Sig::ALRM,
        libc::SIGFPE => Sig::FPE,
        libc::SIGHUP => Sig::HUP,
        libc::SIGILL => Sig::ILL,
        libc::SIGINT => Sig::INT,
        libc::SIGKILL => Sig::KILL,
        libc::SIGPIPE => Sig::PIPE,
        libc::SIGQUIT => Sig::QUIT,
        libc::SIGSEGV => Sig::SEGV,
        libc::SIGTERM => Sig::TERM,
        libc::SIGUSR1 => Sig::USR1,
        other => Sig::Custom(other.to_string()),
    }
}

#[cfg(unix)]
fn signal_number(signal: &Sig) -> Option<libc::c_int> {
    Some(match signal {
        Sig::ABRT => libc::SIGABRT,
        Sig::ALRM => libc::SIGALRM,
        Sig::FPE => libc::SIGFPE,
        Sig::HUP => libc::SIGHUP,
        Sig::ILL => libc::SIGILL,
        Sig::INT => libc::SIGINT,
        Sig::KILL => libc::SIGKILL,
        Sig::PIPE => libc::SIGPIPE,
        Sig::QUIT => libc::SIGQUIT,
        Sig::SEGV => libc::SIGSEGV,
        Sig::TERM => libc::SIGTERM,
        Sig::USR1 => libc::SIGUSR1,
        Sig::Custom(_) => return None,
    })
}

async fn run_pty(
    input: &mut ChannelReadHalf,
    output: &ChannelWriteHalf<Msg>,
    start: Start,
    setup: Setup,
    size: PtySize,
    home: &Path,
) -> Option<Exit> {
    let (program, args) = pty_program(&start);
    let pty = match term::RawPty::spawn(home, program, args, setup.environment) {
        Ok(pty) => pty,
        Err(error) => {
            let message = format!("tcode: could not start the shell: {error}\r\n");
            let _ = output.data_bytes(message.into_bytes()).await;
            return Some(Exit::Status(127));
        }
    };
    let _ = pty.resize(size.cols, size.rows, size.width, size.height);
    if !setup.early.is_empty() {
        let _ = pty.write(setup.early);
    }
    let finished = async {
        loop {
            match pty.next().await {
                Some(term::RawPtyEvent::Output(bytes)) => {
                    if output.data_bytes(bytes).await.is_err() {
                        return Exit::Unknown;
                    }
                }
                Some(term::RawPtyEvent::Exited {
                    exit_code: Some(code),
                }) => return Exit::Status(code as u32),
                Some(term::RawPtyEvent::Exited { exit_code: None }) | None => {
                    return Exit::Unknown;
                }
            }
        }
    };
    let feed = async {
        loop {
            match input.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    let _ = pty.write(data.to_vec());
                }
                Some(ChannelMsg::WindowChange {
                    col_width,
                    row_height,
                    pix_width,
                    pix_height,
                }) => {
                    let size = PtySize::new(col_width, row_height, pix_width, pix_height);
                    let _ = pty.resize(size.cols, size.rows, size.width, size.height);
                }
                Some(ChannelMsg::Signal {
                    signal: Sig::KILL | Sig::TERM | Sig::HUP,
                }) => {
                    let _ = pty.kill();
                }
                Some(ChannelMsg::Close) | None => return,
                Some(_) => {}
            }
        }
    };
    tokio::select! {
        exit = finished => Some(exit),
        () = feed => None,
    }
}

async fn serve_sftp(
    input: &mut ChannelReadHalf,
    output: &ChannelWriteHalf<Msg>,
    setup: Setup,
    home: PathBuf,
) -> Option<Exit> {
    let (ours, theirs) = tokio::io::duplex(256 * 1024);
    russh_sftp::server::run(theirs, sftp::Sftp::new(home)).await;
    let (mut from_sftp, mut to_sftp) = tokio::io::split(ours);
    if !setup.early.is_empty() {
        let _ = to_sftp.write_all(&setup.early).await;
    }
    if setup.eof {
        let _ = to_sftp.shutdown().await;
    }
    let finished = async {
        let _ = tokio::io::copy(&mut from_sftp, &mut output.make_writer()).await;
    };
    let feed = async {
        loop {
            match input.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    let _ = to_sftp.write_all(&data).await;
                }
                Some(ChannelMsg::Eof) => {
                    let _ = to_sftp.shutdown().await;
                }
                Some(ChannelMsg::Close) | None => return,
                Some(_) => {}
            }
        }
    };
    tokio::select! {
        () = finished => Some(Exit::Status(0)),
        () = feed => None,
    }
}
