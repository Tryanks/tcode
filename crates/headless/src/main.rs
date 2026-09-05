use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use tcode_remote::PairingCode;
use tcode_remote::client::{PairInvite, pair_url};
use tcode_remote::discovery::start_beacon;
use tcode_remote::{HostMux, RemoteConfig, serve};
use tcode_runtime::pipe::{HostServices, spawn_host};
use tcode_services::store::SessionStore;

#[cfg(feature = "web")]
const STATIC_BUNDLE: Option<tcode_remote::StaticBundle> = Some(&[
    ("/index.html", include_bytes!("../../web/dist/index.html")),
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
const STATIC_BUNDLE: Option<tcode_remote::StaticBundle> = None;

const DEFAULT_LISTEN: &str = "0.0.0.0:47420";

fn main() {
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
        Some(command) => Err(format!("unknown command {command:?}; use --help")),
    }
}

fn print_usage() {
    println!(
        "Usage:\n  tcode-headless serve [--listen ADDR:PORT] [--name NAME] [--data-dir DIR]\n  tcode-headless pair [--listen ADDR:PORT]\n\nOptions:\n  -h, --help    Print this help"
    );
}

fn serve_command(args: &[String]) -> Result<(), String> {
    let listen = option_value(args, "--listen")
        .unwrap_or_else(|| DEFAULT_LISTEN.to_owned())
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid --listen address: {error}"))?;
    let name = option_value(args, "--name").unwrap_or_else(default_host_name);
    let data_dir = option_value(args, "--data-dir").map(PathBuf::from);
    reject_unknown_options(args, &["--listen", "--name", "--data-dir"])?;
    let store = match data_dir {
        Some(path) => SessionStore::open_at(path),
        None => SessionStore::open_default(),
    }
    .map_err(|error| format!("could not open session store: {error}"))?;
    let remote_data_dir = store.root().clone();
    let mut services = HostServices {
        background_startup_probes: true,
        ai_title_generation: true,
        ..HostServices::default()
    };
    if let Ok(mut mcp_host) = mcp_host::Host::bind() {
        services.orchestrate = Some(orchestrate_mcp::start(&mut mcp_host));
        if let Err(error) = mcp_host.start() {
            eprintln!("tcode-headless: orchestrate MCP server unavailable: {error}");
            services.orchestrate = None;
        }
    }
    let host =
        spawn_host(store, services).map_err(|error| format!("host startup failed: {error}"))?;
    let mux = HostMux::new(host.to_host.clone(), host.from_host.clone());
    let server = serve(
        mux.clone(),
        RemoteConfig {
            listen,
            host_name: name,
            data_dir: remote_data_dir,
            static_bundle: STATIC_BUNDLE,
        },
    )
    .map_err(|error| format!("remote listener failed: {error}"))?;
    let pairing = server.new_pairing_code();
    print_pairing(&pairing)?;
    #[cfg(feature = "web")]
    {
        let bound = server.local_addr();
        if bound.ip().is_unspecified() {
            let mut addrs = pairing.addrs.clone();
            addrs.push(if bound.is_ipv6() { "::1" } else { "127.0.0.1" }.into());
            for addr in addrs {
                if let Ok(ip) = addr.parse::<std::net::IpAddr>() {
                    if ip.is_ipv4() == bound.is_ipv4() {
                        println!("Browser: https://{}/", SocketAddr::new(ip, bound.port()));
                    }
                }
            }
        } else {
            println!("Browser: https://{bound}/");
        }
    }
    let beacon = start_beacon(
        pairing.host_id.clone(),
        pairing.host_name.clone(),
        server.local_addr().port(),
        pairing.fp.clone(),
    );
    println!(
        "Listening on {} (press Ctrl-C to stop)",
        server.local_addr()
    );
    wait_for_interrupt();

    let shutdown_connection = mux.attach();
    let shutdown_id = 1_u64;
    let shutdown_line = serde_json::to_string(&tcode_protocol::ClientMessage {
        id: shutdown_id,
        payload: tcode_protocol::ClientPayload::Command(
            tcode_protocol::Command::ShutdownAllAndFlush,
        ),
    })
    .map_err(|error| error.to_string())?;
    shutdown_connection
        .to_host
        .send_blocking(shutdown_line)
        .map_err(|error| format!("could not request host shutdown: {error}"))?;
    while let Ok(line) = shutdown_connection.from_host.recv_blocking() {
        let Ok(message) = serde_json::from_str::<tcode_protocol::HostMessage>(line.trim_end())
        else {
            continue;
        };
        if matches!(message, tcode_protocol::HostMessage::Ack { id, .. } if id == shutdown_id) {
            break;
        }
    }
    beacon.shutdown();
    server.shutdown();
    host.to_host.close();
    let _ = host.stopped.recv_blocking();
    Ok(())
}

fn pair_command(args: &[String]) -> Result<(), String> {
    let listen = option_value(args, "--listen").unwrap_or_else(|| DEFAULT_LISTEN.to_owned());
    reject_unknown_options(args, &["--listen"])?;
    let address: SocketAddr = listen
        .parse()
        .map_err(|error| format!("invalid --listen address: {error}"))?;
    let loopback = if address.is_ipv6() {
        "::1"
    } else {
        "127.0.0.1"
    };
    let (bytes, _) =
        tcode_remote::client::tls_http(loopback, address.port(), "GET", "/admin/pair", "", "")?;
    let pairing: PairingCode = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    print_pairing(&pairing)
}

fn print_pairing(pairing: &PairingCode) -> Result<(), String> {
    let addrs = if pairing.addrs.is_empty() {
        vec!["127.0.0.1".to_owned()]
    } else {
        pairing.addrs.clone()
    };
    let url = pair_url(&PairInvite {
        host_id: pairing.host_id.clone(),
        name: pairing.host_name.clone(),
        addrs,
        port: pairing.port,
        code: pairing.code.clone(),
        fp: pairing.fp.clone(),
    });
    let qr = QrCode::new(url.as_bytes()).map_err(|error| error.to_string())?;
    println!("Pairing code: {}", pairing.code);
    println!("Fingerprint: {}", pairing.fp);
    println!("Expires in: {} seconds", pairing.expires_in_secs);
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

fn default_host_name() -> String {
    ["HOSTNAME", "HOST", "COMPUTERNAME"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .chain(std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "tcode-host".into())
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn wait_for_interrupt() {
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
