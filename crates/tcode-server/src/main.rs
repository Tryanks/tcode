//! Headless, read-only tcode sync host.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use sync_host::{CommandRequest, WakeSource};
use sync_protocol::{HostInfo, SessionCommand};
use tcode_services::settings::SettingsStore;
use tcode_services::store::SessionStore;

const HELP: &str = "\
tcode-server — serve tcode session history over WebSocket

Usage: tcode-server [OPTIONS]

Options:
  --bind <IP[:PORT]>  Address to listen on [default: 127.0.0.1:0]
                      Port 0 selects an available port. Widening the IP exposes
                      unencrypted traffic; the sync token is the only protection.
  --print-token       Print the sync token to stdout and exit
  -h, --help          Print help
";

const LIVE_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq, Eq)]
struct Options {
    bind: SocketAddr,
    print_token: bool,
    help: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            print_token: false,
            help: false,
        }
    }
}

impl Options {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => options.help = true,
                "--print-token" => options.print_token = true,
                "--bind" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--bind requires an IP address".to_owned())?;
                    options.bind = parse_bind(&value)?;
                }
                _ => {
                    if let Some(value) = arg.strip_prefix("--bind=") {
                        options.bind = parse_bind(value)?;
                    } else {
                        return Err(format!("unknown option: {arg}"));
                    }
                }
            }
        }
        Ok(options)
    }
}

fn parse_bind(value: &str) -> Result<SocketAddr, String> {
    if let Ok(address) = value.parse::<SocketAddr>() {
        return Ok(address);
    }
    value
        .parse::<IpAddr>()
        .map(|ip| SocketAddr::new(ip, 0))
        .map_err(|_| format!("invalid bind address {value:?}; expected IP[:PORT]"))
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("error: {err}\n\n{HELP}");
            std::process::exit(2);
        }
    };
    if options.help {
        print!("{HELP}");
        return;
    }
    if let Err(err) = run(options).await {
        log::error!("tcode-server: {err}");
        std::process::exit(1);
    }
}

async fn run(options: Options) -> io::Result<()> {
    let store = SessionStore::open_default()?;
    let settings = SettingsStore::new(store.root().clone());
    let token = settings.load_or_create_sync_token()?;

    if options.print_token {
        println!("{token}");
        return Ok(());
    }

    let display_name = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "tcode".into());
    let host = HostInfo {
        host_id: format!("{display_name}:{}", store.root().display()),
        display_name,
        platform: std::env::consts::OS.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
    };

    let data_dir = store.root().clone();
    let server = sync_host::start_on(
        store,
        host,
        token,
        options.bind,
        WakeSource::Polling {
            interval: LIVE_POLL_INTERVAL,
        },
    )?;

    log::info!("tcode-server: serving at {}", server.url);
    log::info!("tcode-server: data directory {}", data_dir.display());
    log::warn!("tcode-server: read-only host; serves history and live events but cannot run turns");

    let command_drain = tokio::spawn(drain_commands(server.commands.clone()));
    shutdown_signal().await;
    command_drain.abort();
    Ok(())
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        log::error!("tcode-server: failed to listen for Ctrl-C: {err}");
    }
    log::info!("tcode-server: stopping");
}

async fn drain_commands(commands: async_channel::Receiver<CommandRequest>) {
    while let Ok(request) = commands.recv().await {
        log::warn!(
            "tcode-server: dropped {} command for session {}: no provider process",
            command_kind(&request.command),
            request.session_id
        );
    }
}

fn command_kind(command: &SessionCommand) -> &'static str {
    match command {
        SessionCommand::SendTurn { .. } => "send-turn",
        SessionCommand::Interrupt => "interrupt",
        SessionCommand::RespondApproval { .. } => "approval-response",
        SessionCommand::RespondUserInput { .. } => "user-input-response",
        SessionCommand::SetApprovalMode(_) => "set-approval-mode",
        SessionCommand::Steer { .. } => "steer",
        SessionCommand::SetInteractionMode(_) => "set-interaction-mode",
        SessionCommand::SetOption { .. } => "set-option",
        SessionCommand::Rewind { .. } => "rewind",
        SessionCommand::Shutdown => "shutdown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_an_ephemeral_loopback_port() {
        assert_eq!(
            Options::parse(Vec::<String>::new()).unwrap(),
            Options::default()
        );
    }

    #[test]
    fn bind_accepts_an_ip_with_or_without_a_port() {
        assert_eq!(
            Options::parse(["--bind".into(), "0.0.0.0".into()])
                .unwrap()
                .bind,
            "0.0.0.0:0".parse().unwrap()
        );
        assert_eq!(
            Options::parse(["--bind=[::1]:9876".into()]).unwrap().bind,
            "[::1]:9876".parse().unwrap()
        );
    }

    #[test]
    fn unknown_options_are_rejected() {
        assert_eq!(
            Options::parse(["--listen".into()]).unwrap_err(),
            "unknown option: --listen"
        );
    }
}
