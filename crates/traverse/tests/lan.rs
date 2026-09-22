//! Finding a paired machine again on the LAN: unicast probes over loopback
//! with no multicast dependence, and, opted into with `TCODE_TEST_MDNS=1`,
//! a real DNS-SD advertise and browse on this host's interfaces.
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_lite::StreamExt as _;
use iroh::address_lookup::AddressLookup as _;
use tcode_client::{ConnectionState, host::Transport, pairing::PairedHost};
use tcode_traverse::{
    DeviceIdentity, HostConfig, HostMux, TraverseHost, TraverseMode,
    lan::{self, Browse, DEFAULT_PORT, LanLookup, LanOptions, LocalNetwork},
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tcode-lan-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A machine whose host answers nothing: the handshake is the transport's.
fn start_host(dir: &TestDir, bind_port: Option<u16>) -> TraverseHost {
    let (to_host, _host_rx) = async_channel::unbounded::<String>();
    let (_host_tx, from_host) = async_channel::unbounded::<String>();
    TraverseHost::start(
        HostMux::new(to_host, from_host),
        HostConfig {
            host_name: "LAN Host".into(),
            data_dir: dir.0.clone(),
            traverse: TraverseMode::Off,
            pairing_enabled: true,
            bind_port,
        },
    )
    .unwrap()
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_state(transport: &Transport, wanted: ConnectionState, budget: Duration) -> Duration {
    let started = Instant::now();
    let mut seen = Vec::new();
    while started.elapsed() < budget {
        if let Ok(state) = transport.state.try_recv() {
            if state == wanted {
                return started.elapsed();
            }
            seen.push(state);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("did not observe state {wanted:?}; saw {seen:?}");
}

/// The saved addresses no longer work — the machine restarted on another
/// port — multicast is unavailable, and the device is on the loopback /24:
/// the first probe page carries the machine and the handshake completes
/// against a page of 32 addresses at two ports. The device claims
/// `127.0.0.2` so that `127.0.0.1`, the one loopback address macOS
/// answers on, is probed rather than excluded as its own; on macOS one
/// address of the 64 works, on Linux every one at the right port does.
/// Needs UDP 47420 free: the probes only know the default port.
#[test]
fn a_moved_machine_is_reached_through_the_first_probe_page() {
    let _ = env_logger::builder().is_test(true).try_init();
    let host_dir = TestDir::new("probe-host");
    let host = start_host(&host_dir, Some(DEFAULT_PORT));
    let device_dir = TestDir::new("probe-device");
    let pairing = DeviceIdentity::load_or_create(&device_dir.0).unwrap();
    pairing.set_details("probe".into(), None);
    let minted = host.new_invitation();
    let paired = tcode_traverse::pair_blocking(&minted.invite, &pairing).unwrap();
    // The endpoint that paired remembers the path it used; the launch under
    // test is a fresh one with the same key.
    drop(pairing);
    let device = DeviceIdentity::load_or_create(&device_dir.0).unwrap();
    device.set_lan_options(LanOptions {
        browse: Browse::Off,
        networks: Arc::new(|| {
            vec![LocalNetwork {
                name: "lo0".into(),
                ip: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
                prefix: 24,
            }]
        }),
        multicast_lock: None,
    });
    let stale = PairedHost {
        addrs: vec![format!("127.0.0.1:{}", free_port())],
        ..paired
    };
    tcode_traverse::hosts::save_hosts(&device_dir.0, std::slice::from_ref(&stale)).unwrap();

    let started = Instant::now();
    let client = tcode_traverse::connect(&stale, &device);
    let took = wait_state(&client, ConnectionState::Syncing, Duration::from_secs(15));
    eprintln!("connected through a probe page in {took:?}");
    assert!(
        took < Duration::from_secs(5),
        "the probe page stalled the handshake: {took:?}"
    );
    let reached = host.devices();
    assert!(
        reached[0].live.as_ref().is_some_and(|path| path.direct),
        "the machine sees a direct path: {reached:?}"
    );
    // The address that worked replaces the stale one for the next launch.
    let saved = tcode_traverse::hosts::load_hosts(&device_dir.0).unwrap();
    assert!(
        saved[0]
            .addrs
            .first()
            .is_some_and(|addr| addr.ends_with(&format!(":{DEFAULT_PORT}"))),
        "{:?}",
        saved[0].addrs
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    client.to_host.close();
    host.shutdown();
}

/// The saved addresses come first and at once; with the browse off and no
/// private network attached, that is the whole resolve.
#[test]
fn saved_addresses_are_yielded_first_and_probes_need_an_attached_network() {
    let id = iroh::SecretKey::from_bytes(&[3; 32]).public();
    let saved: Vec<SocketAddr> = vec!["10.0.0.9:47421".parse().unwrap()];
    let lookup = LanLookup::new(
        {
            let saved = saved.clone();
            move |wanted| {
                if wanted == id {
                    saved.clone()
                } else {
                    Vec::new()
                }
            }
        },
        LanOptions {
            browse: Browse::Off,
            networks: Arc::new(Vec::new),
            multicast_lock: None,
        },
    );
    let items = tcode_traverse::block_on(async {
        let mut items = Vec::new();
        let mut stream = lookup.resolve(id).unwrap();
        while let Some(item) = stream.next().await {
            let item = item.unwrap();
            items.push((
                item.provenance(),
                item.endpoint_info().ip_addrs().copied().collect::<Vec<_>>(),
            ));
        }
        items
    });
    assert_eq!(items, [("lan-saved", saved)]);
    assert!(
        lookup
            .resolve(iroh::SecretKey::from_bytes(&[4; 32]).public())
            .is_some()
    );
}

/// Real multicast on this host's interfaces: the machine's DNS-SD record is
/// browsed back with its bound port. Opt in with `TCODE_TEST_MDNS=1`; a CI
/// runner without multicast cannot run it.
#[test]
fn a_machine_advertises_dns_sd_that_a_device_browses() {
    if std::env::var_os("TCODE_TEST_MDNS").is_none_or(|v| v != "1") {
        eprintln!("skipped: set TCODE_TEST_MDNS=1 on a host with multicast");
        return;
    }
    let host_dir = TestDir::new("mdns-host");
    let port = free_port();
    let host = start_host(&host_dir, Some(port));
    let id: iroh::EndpointId = host.endpoint_id().parse().unwrap();
    let lookup = LanLookup::new(
        |_| Vec::new(),
        LanOptions {
            browse: Browse::DnsSd,
            networks: Arc::new(Vec::new),
            multicast_lock: None,
        },
    );
    let started = Instant::now();
    let items = tcode_traverse::block_on(async {
        let mut items = Vec::new();
        let mut stream = lookup.resolve(id).unwrap();
        while let Some(item) = stream.next().await {
            let item = item.unwrap();
            assert_eq!(item.endpoint_id(), id);
            items.push((
                item.provenance(),
                item.endpoint_info().ip_addrs().copied().collect::<Vec<_>>(),
            ));
        }
        items
    });
    eprintln!("DNS-SD resolve took {:?}: {items:?}", started.elapsed());
    let browsed: Vec<SocketAddr> = items
        .iter()
        .filter(|(provenance, _)| *provenance == "lan-dns-sd")
        .flat_map(|(_, addrs)| addrs.clone())
        .collect();
    assert!(!browsed.is_empty(), "no DNS-SD result: {items:?}");
    assert!(browsed.iter().all(|addr| addr.port() == port));
    assert!(browsed.iter().all(|addr| !addr.ip().is_loopback()));
    assert!(
        started.elapsed() <= lan::BROWSE_TIME + Duration::from_secs(1),
        "the browse ends at its deadline"
    );
    host.shutdown();
}
