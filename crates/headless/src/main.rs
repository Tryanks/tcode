use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use tcode_client::pairing::{PairInvite, pair_url, parse_pair_url};
use tcode_runtime::pipe::{HostServices, spawn_host};
use tcode_services::store::SessionStore;
use tcode_traverse::browser::{BrowserConfig, StaticBundle, check_bind, serve, set_password};
use tcode_traverse::identity::write_private;
use tcode_traverse::native_host::default_device_name;
use tcode_traverse::{HostConfig, HostMux, Invitation, TraverseHost, TraverseMode};

#[cfg(feature = "web")]
const STATIC_BUNDLE: Option<StaticBundle> = Some(&[
    ("/index.html", include_bytes!("../../web/dist/index.html")),
    ("/auth.mjs", include_bytes!("../../web/dist/auth.mjs")),
    (
        "/tcode_web.js",
        include_bytes!("../../web/dist/tcode_web.js"),
    ),
    (
        "/tcode_web_bg.wasm",
        include_bytes!("../../web/dist/tcode_web_bg.wasm"),
    ),
]);
#[cfg(not(feature = "web"))]
const STATIC_BUNDLE: Option<StaticBundle> = None;

/// The browser listener stays on loopback unless asked otherwise; devices
/// reach the machine through Traverse.
const DEFAULT_BROWSER_LISTEN: &str = "127.0.0.1:47420";
/// The current invitation, for `pair` to print; absent while none is valid.
const INVITATION_FILE: &str = "invitation.json";

fn main() {
    env_logger::init();
    if let Err(error) = run(std::env::args().skip(1).collect()) {
        eprintln!("tcode-headless: {error}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None | Some("--help" | "-h") => {
            print_usage();
            Ok(())
        }
        Some("serve") => serve_command(&args[1..]),
        Some("pair") => pair_command(&args[1..]),
        Some("set-password") => set_password_command(&args[1..]),
        Some(command) => Err(format!("unknown command {command:?}; use --help")),
    }
}

fn print_usage() {
    println!(
        "Usage:\n  tcode-headless serve [--name NAME] [--data-dir DIR] [--traverse official|off|URL] [--browser-listen ADDR:PORT] [--password PASSWORD]\n  tcode-headless set-password [--data-dir DIR] [--password PASSWORD] [--revoke-tokens]\n  tcode-headless pair [--data-dir DIR]\n\nserve starts this machine on Traverse for native devices and, for browsers,\na plain HTTP listener on {DEFAULT_BROWSER_LISTEN} (--browser-listen binds it\nelsewhere; --listen is accepted as an alias). The browser signs in with a\npassword, set on first open or with --password / TCODE_PASSWORD; a bind\nbeyond loopback is refused until one exists. --traverse selects the relay and\ndiscovery service: official (default), off (invite addresses only),\nor the base URL of a self-hosted instance.\n\npair prints the current invitation link and QR: serve keeps {INVITATION_FILE}\ncurrent, whether the invitation was minted at startup or from a paired\ndevice, and removes it once it is used or expires. Scanning or pasting the\nlink is the whole pairing; an invitation lasts five minutes and admits one\ndevice. A new one comes from a paired device's Settings → Other devices or a\nrestart.\n\nOptions:\n  -h, --help    Print this help"
    );
}

fn parse_traverse(value: Option<String>) -> Result<TraverseMode, String> {
    match value.as_deref() {
        None | Some("official") => Ok(TraverseMode::Official),
        Some("off") => Ok(TraverseMode::Off),
        Some(url) => url::Url::parse(url)
            .map(TraverseMode::Custom)
            .map_err(|error| format!("invalid --traverse value {url:?}: {error}")),
    }
}

fn serve_command(args: &[String]) -> Result<(), String> {
    let browser_listen = option_value(args, "--browser-listen")
        .or_else(|| option_value(args, "--listen"))
        .unwrap_or_else(|| DEFAULT_BROWSER_LISTEN.to_owned())
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid --browser-listen address: {error}"))?;
    let name = option_value(args, "--name").unwrap_or_else(default_device_name);
    let data_dir = option_value(args, "--data-dir").map(PathBuf::from);
    let traverse = parse_traverse(option_value(args, "--traverse"))?;
    reject_unknown_options(
        args,
        &[
            "--listen",
            "--browser-listen",
            "--name",
            "--data-dir",
            "--password",
            "--traverse",
        ],
    )?;
    let store = match data_dir {
        Some(path) => SessionStore::open_at(path),
        None => SessionStore::open_default(),
    }
    .map_err(|error| format!("could not open session store: {error}"))?;
    let remote_data_dir = store.root().clone();
    if let Some(password) =
        option_value(args, "--password").or_else(|| std::env::var("TCODE_PASSWORD").ok())
    {
        set_password(&remote_data_dir, &password, false).map_err(|error| error.to_string())?;
    }
    // Nothing else starts for a bind the listener would refuse anyway.
    check_bind(browser_listen, &remote_data_dir).map_err(|error| error.to_string())?;
    let mut services = HostServices {
        background_startup_probes: true,
        ai_title_generation: true,
        ..HostServices::default()
    };
    if let Ok(mut mcp_host) = mcp_host::Host::bind() {
        services.orchestrate = Some(orchestrate_mcp::start(&mut mcp_host));
        // Preview requests travel to whichever client shows the session's
        // preview panel, so the headless host serves it too.
        services.preview = Some(preview_mcp::start(&mut mcp_host));
        if let Err(error) = mcp_host.start() {
            eprintln!("tcode-headless: MCP servers unavailable: {error}");
            services.orchestrate = None;
            services.preview = None;
        }
    }
    let host =
        spawn_host(store, services).map_err(|error| format!("machine startup failed: {error}"))?;
    let mux = HostMux::new(host.to_host.clone(), host.from_host.clone());
    let relayed = traverse != TraverseMode::Off;
    let traverse_host = Arc::new(
        TraverseHost::start(
            mux.clone(),
            HostConfig {
                host_name: name.clone(),
                data_dir: remote_data_dir.clone(),
                traverse,
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .map_err(|error| format!("could not start Traverse: {error}"))?,
    );
    // The file follows every change for as long as the host runs; the
    // thread ends with the host's event stream. A copy left by a serve that
    // did not shut down goes first.
    let events = traverse_host.invitation_events();
    sync_invitation_file(&remote_data_dir, None)?;
    let invitation_dir = remote_data_dir.clone();
    std::thread::spawn(move || {
        while let Ok(invitation) = events.recv_blocking() {
            if let Err(error) = sync_invitation_file(&invitation_dir, invitation.as_ref()) {
                eprintln!("tcode-headless: {error}");
            }
        }
    });
    let hosting = traverse_host.clone();
    let server = serve(
        mux.clone(),
        BrowserConfig {
            listen: browser_listen,
            host_name: name,
            data_dir: remote_data_dir.clone(),
            static_bundle: STATIC_BUNDLE,
            hosting: Some(Arc::new(move |action| hosting.hosting(action))),
        },
    )
    .map_err(|error| format!("could not listen for browsers: {error}"))?;
    println!("Machine id: {}", traverse_host.endpoint_id());
    if relayed {
        // An invite minted before the relay is known would only carry LAN
        // addresses; wait briefly, never indefinitely.
        traverse_host.wait_online(Duration::from_secs(5));
    }
    if traverse_host.pairing_enabled() {
        print_invitation(&traverse_host.new_invitation())?;
    } else {
        println!("Pairing disabled; enable Allow other devices from a paired client");
    }
    println!(
        "{}",
        if server.password_configured() {
            "Password protected"
        } else {
            "Set a password on first open"
        }
    );
    println!("Browser: http://{}/", server.local_addr());
    println!("Press Ctrl-C to stop");
    wait_for_interrupt();

    let shutdown_connection = mux.attach();
    let shutdown_id = 1_u64;
    let shutdown_line = serde_json::to_string(&tcode_protocol::ClientMessage {
        key: None,
        id: shutdown_id,
        payload: tcode_protocol::ClientPayload::Command(
            tcode_protocol::Command::ShutdownAllAndFlush,
        ),
    })
    .map_err(|error| error.to_string())?;
    shutdown_connection
        .to_host
        .send_blocking(shutdown_line)
        .map_err(|error| format!("could not stop this machine: {error}"))?;
    while let Ok(line) = shutdown_connection.from_host.recv_blocking() {
        let Ok(message) = serde_json::from_str::<tcode_protocol::HostMessage>(line.trim_end())
        else {
            continue;
        };
        if matches!(message, tcode_protocol::HostMessage::Ack { id, .. } if id == shutdown_id) {
            break;
        }
    }
    let _ = std::fs::remove_file(remote_data_dir.join(INVITATION_FILE));
    server.shutdown();
    if let Ok(traverse_host) = Arc::try_unwrap(traverse_host) {
        traverse_host.shutdown();
    }
    host.to_host.close();
    let _ = host.stopped.recv_blocking();
    Ok(())
}

fn set_password_command(args: &[String]) -> Result<(), String> {
    let revoke = args.iter().any(|arg| arg == "--revoke-tokens");
    let values: Vec<_> = args
        .iter()
        .filter(|arg| arg.as_str() != "--revoke-tokens")
        .cloned()
        .collect();
    reject_unknown_options(&values, &["--data-dir", "--password"])?;
    let password = option_value(&values, "--password")
        .or_else(|| std::env::var("TCODE_PASSWORD").ok())
        .ok_or("supply --password or TCODE_PASSWORD")?;
    let store = match option_value(&values, "--data-dir") {
        Some(path) => SessionStore::open_at(PathBuf::from(path)),
        None => SessionStore::open_default(),
    }
    .map_err(|error| error.to_string())?;
    set_password(store.root(), &password, revoke).map_err(|error| error.to_string())?;
    println!(
        "Password changed. {}",
        if revoke {
            "Existing tokens revoked."
        } else {
            "Existing tokens kept."
        }
    );
    Ok(())
}

/// `serve` keeps the current invitation here for `pair` to print.
#[derive(serde::Serialize, serde::Deserialize)]
struct InvitationFile {
    expires_unix: u64,
    invite: String,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Write the invitation in effect, or remove the file when there is none.
fn sync_invitation_file(data_dir: &Path, invitation: Option<&Invitation>) -> Result<(), String> {
    let path = data_dir.join(INVITATION_FILE);
    let Some(invitation) = invitation else {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("could not remove {}: {error}", path.display())),
        };
    };
    let file = InvitationFile {
        expires_unix: now_unix() + invitation.remaining().as_secs(),
        invite: invitation.url(),
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
    write_private(&path, &bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn pair_command(args: &[String]) -> Result<(), String> {
    reject_unknown_options(args, &["--data-dir"])?;
    let store = match option_value(args, "--data-dir") {
        Some(path) => SessionStore::open_at(PathBuf::from(path)),
        None => SessionStore::open_default(),
    }
    .map_err(|error| error.to_string())?;
    let path = store.root().join(INVITATION_FILE);
    let file: Option<InvitationFile> = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let valid = file.filter(|file| file.expires_unix > now_unix());
    let Some(file) = valid else {
        return Err("no valid invitation; a paired device can create one from Settings → Other devices, or restart serve".into());
    };
    let invite = parse_pair_url(&file.invite).ok_or("invalid invitation file")?;
    print_invite(&invite, file.expires_unix - now_unix())
}

fn print_invitation(invitation: &Invitation) -> Result<(), String> {
    print_invite(&invitation.invite, invitation.remaining().as_secs())
}

fn print_invite(invite: &PairInvite, remaining_secs: u64) -> Result<(), String> {
    let url = pair_url(invite);
    let qr = QrCode::new(url.as_bytes()).map_err(|error| error.to_string())?;
    println!("Invitation (scan the QR or paste the link; one device, five minutes):");
    println!("Expires in: {remaining_secs} seconds");
    match &invite.relay {
        Some(relay) => println!("Relay: {relay}"),
        None => println!("Relay: none (LAN only)"),
    }
    println!("Addresses: {}", invite.addrs.join(", "));
    println!("{url}");
    println!("{}", qr.render::<Dense1x2>().quiet_zone(true).build());
    Ok(())
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn reject_unknown_options(args: &[String], options_with_values: &[&str]) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        if options_with_values.contains(&args[index].as_str()) {
            if index + 1 >= args.len() {
                return Err(format!("{} requires a value", args[index]));
            }
            index += 2;
        } else {
            return Err(format!("unknown option {:?}", args[index]));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_interrupt() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    static INTERRUPTED: AtomicBool = AtomicBool::new(false);

    type SignalHandler = extern "C" fn(i32);
    unsafe extern "C" {
        fn signal(signal: i32, handler: SignalHandler) -> SignalHandler;
    }
    extern "C" fn handle_interrupt(_: i32) {
        INTERRUPTED.store(true, Ordering::Relaxed);
    }
    const SIGINT: i32 = 2;
    // SAFETY: installs a process-global handler with the C ABI expected by
    // signal(3); the handler performs only a lock-free atomic store.
    unsafe {
        signal(SIGINT, handle_interrupt);
    }
    while !INTERRUPTED.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(not(unix))]
fn wait_for_interrupt() {
    use std::io::Read as _;
    let _ = std::io::stdin().read(&mut [0_u8]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traverse_flag_selects_official_off_or_a_self_hosted_instance() {
        assert_eq!(parse_traverse(None).unwrap(), TraverseMode::Official);
        assert_eq!(
            parse_traverse(Some("official".into())).unwrap(),
            TraverseMode::Official
        );
        assert_eq!(
            parse_traverse(Some("off".into())).unwrap(),
            TraverseMode::Off
        );
        assert_eq!(
            parse_traverse(Some("https://traverse.example/".into())).unwrap(),
            TraverseMode::Custom(url::Url::parse("https://traverse.example/").unwrap())
        );
        assert!(parse_traverse(Some("not a url".into())).is_err());
    }
}
