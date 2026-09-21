//! Pair with a running machine from its invite link and exchange one
//! `Query`/`QueryResult` over the main stream:
//!
//! ```sh
//! cargo run -p tcode-headless -- serve --name test --data-dir /tmp/tcode-traverse-test
//! cargo run -p tcode-traverse --example pair_and_query -- 'tcode://pair?v=2&...'
//! ```
//!
//! With `TRAVERSE_RELAY_ONLY=1` the device drops the invite's direct
//! addresses and reaches the machine through its Traverse instance's relay
//! and lookup, which exercises the same path a phone on another network
//! takes.
use std::time::{Duration, Instant};

use tcode_client::ConnectionState;
use tcode_client::pairing::parse_pair_url;
use tcode_traverse::DeviceIdentity;

fn main() {
    env_logger::init();
    let Some(link) = std::env::args().nth(1) else {
        eprintln!("usage: pair_and_query <tcode://pair?...>");
        std::process::exit(2);
    };
    let mut invite = parse_pair_url(&link).expect("a v2 invite link");
    let relay_only = std::env::var_os("TRAVERSE_RELAY_ONLY").is_some();
    if relay_only {
        invite.addrs.clear();
    }
    let data_dir = std::env::temp_dir().join("tcode-traverse-example-device");
    let device = DeviceIdentity::load_or_create(&data_dir).expect("device identity");
    device.set_details("example device".into(), None);
    println!("device id {}", device.endpoint_id());
    let started = Instant::now();
    let paired = match tcode_traverse::pair_blocking(&invite, &device) {
        Ok(paired) => paired,
        Err(error) => {
            eprintln!("pairing failed: {error}");
            std::process::exit(1);
        }
    };
    println!(
        "paired with {} ({}) in {:?}",
        paired.name,
        paired.host_id,
        started.elapsed()
    );
    let transport = tcode_traverse::connect(&paired, &device);
    let query = r#"{"id":1,"payload":{"type":"query","content":{"type":"ping"}}}"#;
    transport.to_host.try_send(query.into()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(state) = transport.state.try_recv() {
            println!("state {state:?}");
            if matches!(state, ConnectionState::Offline { .. }) {
                std::process::exit(1);
            }
        }
        if let Ok(line) = transport.from_host.try_recv() {
            print!("host: {line}");
            if line.contains("query_result") {
                println!("round trip complete in {:?}", started.elapsed());
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("no reply within 20 s");
    std::process::exit(1);
}
