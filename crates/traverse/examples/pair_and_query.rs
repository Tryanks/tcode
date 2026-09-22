//! Pair with a running machine from its invite link and exchange one
//! `Query`/`QueryResult` over the main stream, or connect again to a
//! machine paired earlier — after it restarted on another address, say —
//! and show how it was found:
//!
//! ```sh
//! cargo run -p tcode-headless -- serve --name test --data-dir /tmp/tcode-traverse-test
//! cargo run -p tcode-traverse --example pair_and_query -- 'tcode://pair?v=2&...'
//! cargo run -p tcode-traverse --example pair_and_query -- --connect
//! ```
//!
//! With `TRAVERSE_RELAY_ONLY=1` the device drops the invite's direct
//! addresses and reaches the machine through its Traverse instance's relay
//! and lookup, which exercises the same path a phone on another network
//! takes. `RUST_LOG=tcode_traverse=debug` shows the LAN lookup at work.
use std::time::{Duration, Instant};

use tcode_client::ConnectionState;
use tcode_client::pairing::{parse_pair_url, remember_host};
use tcode_traverse::DeviceIdentity;

fn main() {
    env_logger::init();
    let Some(argument) = std::env::args().nth(1) else {
        eprintln!("usage: pair_and_query <tcode://pair?...> | --connect");
        std::process::exit(2);
    };
    let data_dir = std::env::temp_dir().join("tcode-traverse-example-device");
    let device = DeviceIdentity::load_or_create(&data_dir).expect("device identity");
    device.set_details("example device".into(), None);
    println!("device id {}", device.endpoint_id());
    let started = Instant::now();
    let paired = if argument == "--connect" {
        let hosts = tcode_traverse::hosts::load_hosts(&data_dir).expect("hosts.json");
        let Some(saved) = hosts.into_iter().last() else {
            eprintln!("no machine paired yet; pair with an invite link first");
            std::process::exit(1);
        };
        println!(
            "connecting to {} ({}) last seen at {:?}",
            saved.name, saved.host_id, saved.addrs
        );
        saved
    } else {
        let mut invite = parse_pair_url(&argument).expect("a v2 invite link");
        if std::env::var_os("TRAVERSE_RELAY_ONLY").is_some() {
            invite.addrs.clear();
        }
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
        let remembered = paired.clone();
        tcode_traverse::hosts::update_hosts(&data_dir, |hosts| remember_host(hosts, remembered))
            .expect("hosts.json");
        paired
    };
    let transport = tcode_traverse::connect(&paired, &device);
    let query = r#"{"id":1,"payload":{"type":"query","content":{"type":"ping"}}}"#;
    transport.to_host.try_send(query.into()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(state) = transport.state.try_recv() {
            println!("state {state:?} at {:?}", started.elapsed());
            if matches!(state, ConnectionState::Offline { .. }) {
                std::process::exit(1);
            }
        }
        if let Ok(line) = transport.from_host.try_recv() {
            print!("host: {line}");
            if line.contains("query_result") {
                let live = transport.current_host.as_ref().expect("a live host");
                println!(
                    "round trip complete in {:?}; now saved at {:?}",
                    started.elapsed(),
                    live.snapshot().addrs
                );
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("no reply within 30 s");
    std::process::exit(1);
}
