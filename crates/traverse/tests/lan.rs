//! Finding a paired machine again on the LAN: the saved addresses over
//! loopback with the browse off, and an explicitly ignored real DNS-SD
//! advertise and browse on this host's interfaces.
use std::{
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};

use futures_lite::StreamExt as _;
use iroh::address_lookup::AddressLookup as _;
use tcode_client::{ConnectionState, host::Transport, pairing::PairedHost};
use tcode_traverse::{
    DeviceIdentity, HostConfig, HostMux, TraverseHost,
    lan::{self, Browse, LanLookup, LanOptions},
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
fn start_host(
    dir: &TestDir,
    bind_port: Option<u16>,
) -> (
    TraverseHost,
    async_channel::Receiver<tcode_client::attachment::AttachmentMessage>,
    async_channel::Sender<String>,
) {
    let (to_host, host_rx) =
        async_channel::unbounded::<tcode_client::attachment::AttachmentMessage>();
    let (host_tx, from_host) = async_channel::unbounded::<String>();
    let host = TraverseHost::start(
        HostMux::new(to_host, from_host),
        HostConfig {
            host_name: "LAN Host".into(),
            data_dir: dir.0.clone(),
            traverse: Vec::new(),
            pairing_enabled: true,
            bind_port,
        },
    )
    .unwrap();
    (host, host_rx, host_tx)
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_state(transport: &Transport, wanted: impl Fn(&ConnectionState) -> bool) {
    let mut seen = Vec::new();
    tcode_traverse::block_on(async {
        tokio::time::timeout(Duration::from_secs(20), async {
            while let Ok(state) = transport.state.recv().await {
                if wanted(&state) {
                    return;
                }
                seen.push(state);
            }
        })
        .await
    })
    .unwrap_or_else(|_| panic!("did not observe the wanted state; saw {seen:?}"));
}

/// A device paired with `host` whose `hosts.json` lists `addrs` for it, as
/// a fresh launch sees it: the endpoint that paired is gone, the browse is
/// off, so the saved addresses are the whole lookup.
fn relaunched_device(
    host: &TraverseHost,
    dir: &TestDir,
    addrs: Vec<String>,
) -> (DeviceIdentity, PairedHost) {
    let pairing = DeviceIdentity::load_or_create(&dir.0).unwrap();
    pairing.set_details("device".into(), None);
    let minted = host.new_invitation();
    let paired = tcode_traverse::pair_blocking(
        &minted.invite,
        Some(std::net::Ipv4Addr::LOCALHOST.into()),
        &pairing,
    )
    .unwrap();
    drop(pairing);
    let device = DeviceIdentity::load_or_create(&dir.0).unwrap();
    device.set_lan_options(LanOptions {
        browse: Browse::Off,
        multicast_lock: None,
    });
    let saved = PairedHost { addrs, ..paired };
    tcode_traverse::hosts::save_hosts(&dir.0, std::slice::from_ref(&saved)).unwrap();
    (device, saved)
}

#[test]
fn a_saved_address_further_down_the_list_reaches_the_machine() {
    let _ = env_logger::builder().is_test(true).try_init();
    let host_dir = TestDir::new("saved-host");
    let port = free_port();
    let (host, _host_rx, _host_tx) = start_host(&host_dir, Some(port));
    let device_dir = TestDir::new("saved-device");
    let current = format!("127.0.0.1:{port}");
    let (device, saved) = relaunched_device(
        &host,
        &device_dir,
        vec![
            format!("127.0.0.1:{}", free_port()),
            format!("127.0.0.1:{}", free_port()),
            current.clone(),
        ],
    );

    let client = tcode_traverse::connect(&saved, &device);
    wait_state(&client, |state| {
        matches!(state, ConnectionState::Syncing { .. })
    });
    let reached = host.devices();
    assert!(
        reached[0].live.as_ref().is_some_and(|path| path.direct),
        "the machine sees a direct path: {reached:?}"
    );
    // The link saves the addresses that reached the machine before it
    // reports Syncing.
    let saved = tcode_traverse::hosts::load_hosts(&device_dir.0).unwrap();
    assert!(
        saved[0]
            .addrs
            .first()
            .is_some_and(|addr| addr.ends_with(&format!(":{port}"))),
        "working address was not saved first: {:?}",
        saved[0].addrs
    );
    client.to_host.close();
    host.shutdown();
}

/// A platform browse delivers what it finds while the resolve is polled, and
/// is told to stop the moment the resolve is dropped: iroh drops a resolve
/// as soon as it has a path, which aborts the task behind it, so the stop
/// must not wait for the task's deadline.
#[test]
fn a_dropped_resolve_stops_the_platform_browse_at_once() {
    let id = iroh::SecretKey::from_bytes(&[5; 32]).public();
    let (started_tx, started) = async_channel::unbounded();
    let (stopped_tx, stopped) = async_channel::unbounded();
    let browser = std::sync::Arc::new(lan::SystemBrowser::new(
        move |request| started_tx.send_blocking(request).unwrap(),
        move |request| stopped_tx.send_blocking(request).unwrap(),
    ));
    let lookup = LanLookup::new(
        |_| Vec::new(),
        LanOptions {
            browse: Browse::System(browser.clone()),
            multicast_lock: None,
        },
    );
    let found: SocketAddr = "10.0.0.12:47420".parse().unwrap();
    tcode_traverse::block_on(async {
        let mut stream = lookup.resolve(id).unwrap();
        let request = started.recv().await.unwrap();
        browser.found(request, &id.to_string(), [found]);
        let item = stream.next().await.unwrap().unwrap();
        assert_eq!(item.provenance(), "lan-dns-sd");
        assert_eq!(
            item.endpoint_info().ip_addrs().copied().collect::<Vec<_>>(),
            [found]
        );
        assert!(stopped.is_empty(), "the browse runs on");
        let dropped_at = Instant::now();
        drop(stream);
        let stop = stopped.recv().await.unwrap();
        assert_eq!(stop, request);
        assert!(
            dropped_at.elapsed() < lan::BROWSE_TIME,
            "the platform browse was stopped before its deadline"
        );
        assert!(started.is_empty(), "one browse was started");
    });
}

/// Real multicast on this host's interfaces: the machine's DNS-SD record is
/// browsed back with its bound port. Run explicitly with `--ignored` on a
/// host with multicast; a CI runner without multicast cannot run it.
#[test]
#[ignore = "requires multicast on a real interface"]
fn a_machine_advertises_dns_sd_that_a_device_browses() {
    let host_dir = TestDir::new("mdns-host");
    let port = free_port();
    let (host, _host_rx, _host_tx) = start_host(&host_dir, Some(port));
    let id: iroh::EndpointId = host.endpoint_id().parse().unwrap();
    let lookup = LanLookup::new(
        |_| Vec::new(),
        LanOptions {
            browse: Browse::DnsSd,
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
