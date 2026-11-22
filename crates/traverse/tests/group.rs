//! Machine groups over real loopback endpoints: two machines with Traverse
//! off, a desktop that hosts beside its own window, and phones that pair
//! with one member and reach the others.
use std::path::PathBuf;

use tcode_client::ConnectionState;
use tcode_client::host::Transport;
use tcode_client::pairing::{PairInvite, PairedHost, parse_pair_url};
use tcode_protocol::{GroupAction, HostingAction, HostingState};
use tcode_traverse::{DeviceIdentity, HostConfig, HostMux, TraverseHost};

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tcode-group-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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

/// A machine whose application side is never asked anything: these tests
/// only open connections.
struct Machine {
    host: TraverseHost,
    _channels: (
        async_channel::Receiver<tcode_client::attachment::AttachmentMessage>,
        async_channel::Sender<String>,
    ),
}

fn machine(dir: &TestDir, name: &str) -> Machine {
    let (to_host, host_rx) =
        async_channel::unbounded::<tcode_client::attachment::AttachmentMessage>();
    let (host_tx, from_host) = async_channel::unbounded::<String>();
    let host = TraverseHost::start(
        HostMux::new(to_host, from_host),
        HostConfig {
            host_name: name.into(),
            data_dir: dir.0.clone(),
            traverse: Vec::new(),
            pairing_enabled: true,
            bind_port: None,
        },
    )
    .unwrap();
    Machine {
        host,
        _channels: (host_rx, host_tx),
    }
}

/// A device that finds machines only at the addresses it saved, as a phone
/// off any service does once nothing else answers.
fn device(dir: &TestDir, name: &str) -> DeviceIdentity {
    let device = DeviceIdentity::load_or_create(&dir.0).unwrap();
    device.set_details(name.into(), None);
    device.set_lan_options(tcode_traverse::lan::LanOptions {
        browse: tcode_traverse::lan::Browse::Off,
        multicast_lock: None,
    });
    device
}

fn group(machine: &Machine, action: GroupAction) -> HostingState {
    machine
        .host
        .hosting(HostingAction::Group(action))
        .unwrap_or_else(|error| panic!("{error:?}"))
}

fn state(machine: &Machine) -> HostingState {
    machine.host.hosting(HostingAction::State).unwrap()
}

/// A fresh one-time invitation, as Settings → Remote shows it.
fn invitation(machine: &Machine) -> String {
    machine.host.new_invitation().url()
}

/// Pair with an invitation at the address typed for it, and save the
/// machine as the pairing screen does.
fn pair(link: &str, device: &DeviceIdentity) -> PairedHost {
    let invite: PairInvite = parse_pair_url(link).unwrap();
    let paired =
        tcode_traverse::pair_blocking(&invite, Some(std::net::Ipv4Addr::LOCALHOST.into()), device)
            .unwrap();
    tcode_traverse::hosts::update_hosts(device.data_dir(), |hosts| {
        tcode_client::pairing::remember_host(hosts, paired.clone());
    })
    .unwrap();
    paired
}

fn saved(device: &DeviceIdentity, machine: &Machine) -> PairedHost {
    let id = machine.host.endpoint_id();
    tcode_traverse::hosts::load_hosts(device.data_dir())
        .unwrap()
        .into_iter()
        .find(|host| host.host_id == id)
        .unwrap_or_else(|| panic!("{id} is not saved"))
}

/// The next message on `channel`, or `None` once it has closed. A message
/// that never comes is ended by the test runner.
fn next<T>(channel: &async_channel::Receiver<T>) -> Option<T> {
    channel.recv_blocking().ok()
}

/// Waits until `check` holds on `machine`, which says so through its hosting
/// state as it changes.
fn eventually(machine: &Machine, what: &str, check: impl Fn() -> bool) {
    let changes = machine.host.hosting_events();
    while !check() {
        next(&changes).unwrap_or_else(|| panic!("never saw {what}"));
    }
}

fn admits(machine: &Machine, device: &DeviceIdentity) -> bool {
    let id = device.endpoint_id().to_string();
    machine.host.devices().iter().any(|record| record.id == id)
}

fn wait_state(transport: &Transport, wanted: impl Fn(&ConnectionState) -> bool) {
    let mut seen = Vec::new();
    while let Some(state) = next(&transport.state) {
        if wanted(&state) {
            return;
        }
        seen.push(state);
    }
    panic!("did not observe the wanted state; saw {seen:?}");
}

fn up(state: &ConnectionState) -> bool {
    matches!(
        state,
        ConnectionState::Syncing { .. } | ConnectionState::Connected { .. }
    )
}

/// #935's scenario: the headless machine starts a group with the device
/// already paired to it, the desktop joins with the headless machine's
/// invitation and brings its window, a phone pairs with the desktop's
/// invitation and reaches the headless machine too; once the headless
/// machine has left, the phone still reaches the desktop, and a new phone
/// pairs with the desktop alone.
#[test]
fn a_device_paired_with_one_member_reaches_every_member_and_outlives_the_first() {
    let headless_dir = TestDir::new("headless");
    let headless = machine(&headless_dir, "Docker");
    let desktop_dir = TestDir::new("desktop");
    let window = device(&desktop_dir, "Desktop window");
    let desktop = machine(&desktop_dir, "Desktop");
    let early_dir = TestDir::new("early");
    let early = device(&early_dir, "Early phone");
    pair(&invitation(&headless), &early);

    let created = group(
        &headless,
        GroupAction::Create {
            name: "Home".into(),
        },
    );
    let home = created.group.unwrap();
    assert_eq!(home.machines.len(), 1);
    assert!(home.machines[0].this_machine);
    let link = invitation(&headless);
    assert_eq!(parse_pair_url(&link).unwrap().group, Some(home.id.clone()));
    let saved_changes = tcode_traverse::hosts::watch(&desktop_dir.0);
    let joined = group(
        &desktop,
        GroupAction::Join {
            link,
            address: Some("127.0.0.1".into()),
        },
    );
    assert_eq!(joined.group.as_ref().unwrap().machines.len(), 2);
    assert_eq!(state(&headless).group.unwrap().machines.len(), 2);
    assert!(
        admits(&headless, &window),
        "the desktop brings its own window"
    );
    assert!(
        !admits(&desktop, &window),
        "the window reaches its own machine without the network"
    );
    assert!(
        admits(&desktop, &early),
        "a device paired before the group was created is a member"
    );
    let from_window = saved(&window, &headless);
    assert_eq!(from_window.group_id.as_deref(), Some(home.id.as_str()));
    assert_eq!(from_window.group_name.as_deref(), Some("Home"));
    assert!(
        saved_changes.try_recv().is_ok(),
        "the window hears of the group's machines as they are saved"
    );
    let client = tcode_traverse::connect(&from_window, &window);
    wait_state(&client, up);
    client.to_host.close();

    let phone_dir = TestDir::new("phone");
    let phone = device(&phone_dir, "Phone");
    let paired = pair(&invitation(&desktop), &phone);
    assert_eq!(paired.name, "Desktop");
    assert_eq!(paired.group_id.as_deref(), Some(home.id.as_str()));
    assert_eq!(paired.group_name.as_deref(), Some("Home"));
    assert!(admits(&desktop, &phone));
    eventually(&headless, "the headless machine admit the phone", || {
        admits(&headless, &phone)
    });
    let to_headless = tcode_traverse::connect(&saved(&phone, &headless), &phone);
    wait_state(&to_headless, up);
    to_headless.to_host.close();

    group(&headless, GroupAction::Leave);
    assert!(state(&headless).group.is_none());
    assert!(
        admits(&headless, &phone),
        "a machine that leaves keeps its devices as directly paired"
    );
    eventually(
        &desktop,
        "the desktop see the headless machine leave",
        || state(&desktop).group.unwrap().machines.len() == 1,
    );
    headless.host.shutdown();
    let to_desktop = tcode_traverse::connect(&saved(&phone, &desktop), &phone);
    wait_state(&to_desktop, up);
    to_desktop.to_host.close();

    let second_dir = TestDir::new("second-phone");
    let second = device(&second_dir, "Second phone");
    pair(&invitation(&desktop), &second);
    assert!(admits(&desktop, &second));
    desktop.host.shutdown();
}
