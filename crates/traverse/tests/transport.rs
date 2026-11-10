//! Two in-process endpoints using native direct-path selection, without a relay.
#[path = "common/original.rs"]
mod original_fixture;
#[path = "common/host.rs"]
mod real_host;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tcode_client::host::{StreamTarget, Transport};
use tcode_client::pairing::{PairInvite, PairedHost};
use tcode_client::recovery::Wake;
use tcode_client::{ConnectionFailure, ConnectionState};
use tcode_traverse::manifest::ManifestSource;
use tcode_traverse::{DeviceIdentity, HostConfig, HostMux, PairError, TraverseHost};

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tcode-traverse-{tag}-{}-{}",
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

/// A machine whose host is never asked anything: these tests only pair,
/// connect and use what the transport answers itself.
struct Silent {
    mux: HostMux,
    _host: (
        async_channel::Receiver<tcode_client::attachment::AttachmentMessage>,
        async_channel::Sender<String>,
    ),
}

fn silent_host() -> Silent {
    let (to_host, host_rx) = async_channel::unbounded();
    let (host_tx, from_host) = async_channel::unbounded();
    Silent {
        mux: HostMux::new(to_host, from_host),
        _host: (host_rx, host_tx),
    }
}

/// The machine's own host on an empty store, for tests whose devices are
/// answered by it.
fn running_host(dir: &TestDir) -> (tcode_runtime::pipe::SpawnedHost, HostMux) {
    real_host::spawn(&dir.0, Default::default(), |_| {})
}

fn start_host(mux: HostMux, dir: &TestDir, bind_port: Option<u16>) -> TraverseHost {
    TraverseHost::start(
        mux,
        HostConfig {
            host_name: "Test Host".into(),
            data_dir: dir.0.clone(),
            traverse: Vec::new(),
            pairing_enabled: true,
            bind_port,
        },
    )
    .unwrap()
}

fn device(dir: &TestDir, name: &str) -> DeviceIdentity {
    let device = DeviceIdentity::load_or_create(&dir.0).unwrap();
    device.set_details(name.into(), None);
    device
}

/// Pair as a person does once nothing found the machine: these machines
/// run Off on loopback and a link names no address, so the address typed
/// for the link's port is the path that does not depend on DNS-SD.
fn pair(invite: &PairInvite, device: &DeviceIdentity) -> Result<PairedHost, PairError> {
    tcode_traverse::pair_blocking(invite, Some(std::net::Ipv4Addr::LOCALHOST.into()), device)
}

// Native multipath may select a global interface even between local peers.
fn connected_directly(state: &ConnectionState) -> bool {
    matches!(
        state,
        ConnectionState::Connected {
            path: Some(tcode_protocol::PathInfo {
                direct: true,
                relay: None,
                probing_direct: false,
                ..
            }),
        }
    )
}

fn syncing_directly(state: &ConnectionState) -> bool {
    matches!(
        state,
        ConnectionState::Syncing {
            path: Some(tcode_protocol::PathInfo {
                direct: true,
                relay: None,
                probing_direct: false,
                ..
            }),
        }
    )
}

/// The next message on `channel`, or `None` once it has closed. A message
/// that never comes is ended by the test runner.
fn next<T>(channel: &async_channel::Receiver<T>) -> Option<T> {
    channel.recv_blocking().ok()
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

fn recv_type(transport: &Transport, kind: &str, id: Option<u64>) -> Value {
    while let Some(line) = next(&transport.from_host) {
        let value: Value = serde_json::from_str(line.trim_end()).unwrap();
        if value["type"] == kind && id.is_none_or(|id| value["content"]["id"].as_u64() == Some(id))
        {
            return value;
        }
    }
    panic!("did not receive {kind}");
}

fn subscribe(id: u64) -> String {
    tcode_protocol::encode_line(&tcode_protocol::ClientMessage {
        id,
        key: None,
        principal: None,
        payload: tcode_protocol::ClientPayload::Subscribe(tcode_protocol::Subscription {
            known_index: None,
            topic: tcode_protocol::Topic::Index,
            after: None,
        }),
    })
    .unwrap()
}

#[test]
fn invitations_are_single_use_five_wrong_secrets_invalidate_and_unpaired_devices_are_rejected() {
    let host_dir = TestDir::new("pair-host");
    let silent = silent_host();
    let host = start_host(silent.mux.clone(), &host_dir, None);
    let phone_dir = TestDir::new("pair-phone");
    let phone = device(&phone_dir, "phone");
    let other_dir = TestDir::new("pair-other");
    let other = device(&other_dir, "other");

    let minted = host.new_invitation();
    let invite = &minted.invite;
    assert_eq!(invite.host_id, host.endpoint_id());
    assert!(tcode_client::pairing::valid_invitation_secret(
        &invite.secret
    ));
    let link = tcode_client::pairing::parse_pair_url(&minted.url()).unwrap();
    assert_eq!(&link, invite, "the link carries the whole invitation");
    let wrong = PairInvite {
        secret: "AAAAAAAAAAAAAAAAAAAAAA".into(),
        ..invite.clone()
    };
    for _ in 0..5 {
        assert_eq!(pair(&wrong, &phone), Err(PairError::Invalid));
    }
    assert_eq!(
        pair(invite, &phone),
        Err(PairError::Invalid),
        "five wrong secrets burn the invitation even for the right one"
    );
    assert!(host.devices().is_empty());
    assert!(host.invitation().is_none());

    let old = host.new_invitation();
    let minted = host.new_invitation();
    assert_ne!(old.invite.secret, minted.invite.secret);
    assert_eq!(
        pair(&old.invite, &phone),
        Err(PairError::Invalid),
        "a new invitation replaces the old one"
    );
    assert_eq!(
        host.invitation().map(|(active, _)| active.invite),
        Some(minted.invite.clone())
    );
    let paired = pair(&minted.invite, &phone).unwrap();
    assert_eq!(paired.host_id, host.endpoint_id());
    assert_eq!(paired.name, "Test Host");
    assert_eq!(
        pair(&minted.invite, &other),
        Err(PairError::Invalid),
        "an invitation is single use"
    );
    assert!(host.invitation().is_none(), "a used invitation is gone");
    let devices = host.devices();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].id, phone.endpoint_id().to_string());
    assert_eq!(devices[0].name, "phone");
    let stored: Value =
        serde_json::from_slice(&std::fs::read(host_dir.0.join("traverse.json")).unwrap()).unwrap();
    assert_eq!(stored["devices"][0]["id"], phone.endpoint_id().to_string());

    let stranger = tcode_traverse::connect(&paired, &other);
    wait_state(&stranger, |state| {
        state
            == &ConnectionState::Offline {
                reason: ConnectionFailure::AuthenticationRejected,
            }
    });
    stranger.to_host.close();

    // A switch the allow list cannot record is answered as a failure and
    // changes nothing. The file is written through `traverse.tmp`; a
    // directory in its place fails the write on every platform.
    let blocker = host_dir.0.join("traverse.tmp");
    std::fs::create_dir(&blocker).unwrap();
    assert!(matches!(
        host.hosting(tcode_protocol::HostingAction::SetEnabled(false)),
        Err(tcode_protocol::ProtocolError { code, .. }) if code == "pairing_update_failed"
    ));
    assert!(host.pairing_enabled());
    std::fs::remove_dir(&blocker).unwrap();

    host.set_pairing_enabled(false);
    let minted = host.new_invitation();
    assert_eq!(pair(&minted.invite, &other), Err(PairError::Disabled));
    host.shutdown();
}

/// A LAN-only machine the browse cannot find: the link names no address,
/// so the first attempt opens no path and sends no secret. The same
/// invitation then pairs at the address the person types, on the link's
/// port, and the paired machine is reached again from its saved addresses,
/// with nothing else to find it by.
#[test]
fn an_invitation_that_finds_no_path_pairs_at_a_typed_address() {
    let host_dir = TestDir::new("typed-host");
    let silent = silent_host();
    let host = start_host(silent.mux.clone(), &host_dir, None);
    let dir = TestDir::new("typed-phone");
    let phone = device(&dir, "phone");
    phone.set_lan_options(tcode_traverse::lan::LanOptions {
        browse: tcode_traverse::lan::Browse::Off,
        multicast_lock: None,
    });
    let link = tcode_client::pairing::parse_pair_url(&host.new_invitation().url()).unwrap();

    let started = Instant::now();
    let failure = tcode_traverse::pair_blocking(&link, None, &phone).unwrap_err();
    assert!(
        matches!(failure, PairError::Unreachable(_)),
        "{failure:?} after {:?}",
        started.elapsed()
    );
    assert!(host.invitation().is_some(), "the secret was never sent");

    let typed = std::net::Ipv4Addr::LOCALHOST.into();
    let paired = tcode_traverse::pair_blocking(&link, Some(typed), &phone).unwrap();
    assert_eq!(paired.name, "Test Host");
    let worked = std::net::SocketAddr::new(typed, link.port).to_string();
    assert!(paired.addrs.contains(&worked), "{:?}", paired.addrs);
    assert_eq!(paired.addrs.first(), Some(&worked));
    assert!(host.invitation().is_none(), "the invitation is used");

    tcode_traverse::hosts::save_hosts(&dir.0, std::slice::from_ref(&paired)).unwrap();
    drop(phone);
    let phone = DeviceIdentity::load_or_create(&dir.0).unwrap();
    phone.set_lan_options(tcode_traverse::lan::LanOptions {
        browse: tcode_traverse::lan::Browse::Off,
        multicast_lock: None,
    });
    let saved = tcode_traverse::hosts::load_hosts(&dir.0).unwrap();
    let client = tcode_traverse::connect(&saved[0], &phone);
    wait_state(&client, syncing_directly);
    client.to_host.close();
    host.shutdown();
}

/// Whoever keeps a copy of the invitation hears every change, in order:
/// each mint, the burn after five wrong secrets, its use and pairing being
/// turned off. A listener added later hears only what follows.
#[test]
fn invitation_changes_are_reported_in_order() {
    let host_dir = TestDir::new("events-host");
    let silent = silent_host();
    let host = start_host(silent.mux.clone(), &host_dir, None);
    let phone_dir = TestDir::new("events-phone");
    let phone = device(&phone_dir, "phone");
    let events = host.invitation_events();

    let first = host.new_invitation();
    assert_eq!(events.recv_blocking().unwrap(), Some(first));
    let second = host.new_invitation();
    assert_eq!(events.recv_blocking().unwrap(), Some(second.clone()));
    let wrong = PairInvite {
        secret: "AAAAAAAAAAAAAAAAAAAAAA".into(),
        ..second.invite
    };
    for attempt in 0..5 {
        assert_eq!(pair(&wrong, &phone), Err(PairError::Invalid));
        if attempt < 4 {
            assert!(events.try_recv().is_err(), "a wrong secret changes nothing");
        }
    }
    assert_eq!(
        events.recv_blocking().unwrap(),
        None,
        "five wrong secrets burn it"
    );
    let third = host.new_invitation();
    assert_eq!(events.recv_blocking().unwrap(), Some(third.clone()));
    pair(&third.invite, &phone).unwrap();
    assert_eq!(events.recv_blocking().unwrap(), None, "used");
    host.set_pairing_enabled(false);
    assert!(events.try_recv().is_err(), "nothing to withdraw");
    host.set_pairing_enabled(true);
    let late = host.invitation_events();
    let fourth = host.new_invitation();
    assert_eq!(events.recv_blocking().unwrap(), Some(fourth.clone()));
    assert_eq!(late.recv_blocking().unwrap(), Some(fourth));
    host.set_pairing_enabled(false);
    assert_eq!(events.recv_blocking().unwrap(), None, "pairing off");
    assert_eq!(late.recv_blocking().unwrap(), None);
    assert!(events.try_recv().is_err());
    host.shutdown();
}

/// A fresh manifest cache for an instance whose relay is
/// `https://{name}.relay.test/`, so no test fetches a manifest.
fn cache_manifest(path: &std::path::Path, name: &str) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::write(
        path,
        json!({
            "fetchedAtMs": now_ms,
            "manifest": {
                "version": 1,
                "updatedAt": "2999-01-01T00:00:00Z",
                "relays": [{"url": format!("https://{name}.relay.test/")}],
                "pkarr": [format!("https://{name}.lookup.test/pkarr")]
            }
        })
        .to_string(),
    )
    .unwrap();
}

fn wait_relays(device: &DeviceIdentity, wanted: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = device.relays();
    while seen != wanted && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        seen = device.relays();
    }
    assert_eq!(seen, wanted);
}

/// A device's relays are those of its machines' Traverse instances: a
/// device whose only machine is Off carries none, a machine on the official
/// service brings the official relays, a custom instance adds its own, and
/// removing those machines takes their relays away again. Fresh manifest
/// caches keep discovery local to the test.
#[test]
fn device_relays_follow_the_traverse_instances_of_its_machines() {
    let silent = silent_host();
    let off_dir = TestDir::new("relays-off-host");
    let off_host = start_host(silent.mux.clone(), &off_dir, None);
    let official_dir = TestDir::new("relays-official-host");
    let official_host = start_host(silent.mux.clone(), &official_dir, None);
    let custom_dir = TestDir::new("relays-custom-host");
    let custom_host = start_host(silent.mux.clone(), &custom_dir, None);
    let dir = TestDir::new("relays-phone");
    cache_manifest(&dir.0.join("traverse-manifest-official.json"), "official");
    let custom_base: url::Url = "https://custom.traverse.test/".parse().unwrap();
    cache_manifest(
        &tcode_traverse::manifest::ManifestSource::Custom(custom_base.clone()).cache_path(&dir.0),
        "custom",
    );
    let phone = device(&dir, "phone");

    let minted = off_host.new_invitation();
    assert_eq!(
        minted.invite.traverse,
        [tcode_client::pairing::TRAVERSE_OFF],
        "an Off machine says so in its invitation"
    );
    let off = pair(&minted.invite, &phone).unwrap();
    tcode_traverse::hosts::save_hosts(&dir.0, std::slice::from_ref(&off)).unwrap();
    wait_relays(&phone, &[]);

    // The test machine runs Off; its invitation is rewritten to claim the
    // official service, which the machine never checks.
    let minted = official_host.new_invitation();
    let invite = PairInvite {
        traverse: Vec::new(),
        ..minted.invite
    };
    let official = pair(&invite, &phone).unwrap();
    assert!(official.traverse.is_empty());
    wait_relays(&phone, &["https://official.relay.test/"]);
    // Pair this machine while Off, so its custom source can only be loaded
    // by the saved-host reconciliation below, not by pairing itself.
    let mut custom = pair(&custom_host.new_invitation().invite, &phone).unwrap();
    custom.traverse = vec![custom_base.to_string()];
    tcode_traverse::hosts::save_hosts(&dir.0, &[off.clone(), official, custom]).unwrap();
    phone.hosts_changed();
    // The new relay proves reconciliation ran; the saved official source
    // must survive that same update.
    wait_relays(
        &phone,
        &["https://custom.relay.test/", "https://official.relay.test/"],
    );

    tcode_traverse::hosts::save_hosts(&dir.0, std::slice::from_ref(&off)).unwrap();
    phone.hosts_changed();
    wait_relays(&phone, &[]);

    // A machine that publishes to several instances lists them all, and
    // pairing with it brings every one's relays.
    let minted = official_host.new_invitation();
    let invite = PairInvite {
        traverse: vec![
            tcode_client::pairing::TRAVERSE_OFFICIAL.into(),
            custom_base.to_string(),
        ],
        ..minted.invite
    };
    let both = pair(&invite, &phone).unwrap();
    tcode_traverse::hosts::save_hosts(&dir.0, &[off.clone(), both]).unwrap();
    phone.hosts_changed();
    wait_relays(
        &phone,
        &["https://custom.relay.test/", "https://official.relay.test/"],
    );
    tcode_traverse::hosts::save_hosts(&dir.0, std::slice::from_ref(&off)).unwrap();
    phone.hosts_changed();
    wait_relays(&phone, &[]);

    // A pairing that fails leaves nothing of the instance it tried behind.
    let minted = official_host.new_invitation();
    let wrong = PairInvite {
        traverse: Vec::new(),
        secret: "AAAAAAAAAAAAAAAAAAAAAA".into(),
        ..minted.invite
    };
    assert_eq!(pair(&wrong, &phone), Err(PairError::Invalid));
    wait_relays(&phone, &[]);
    off_host.shutdown();
    official_host.shutdown();
    custom_host.shutdown();
}

/// A self-hosted instance that cannot be reached is not a reason to stay
/// off the LAN: the machine hosts with no relay and no lookup, the device
/// fetches the same manifest and fails the same way, and pairing and
/// direct connections still work. The official service is never
/// substituted on either side.
#[test]
fn an_unreachable_self_hosted_instance_still_lets_the_lan_pair_and_connect() {
    let host_dir = TestDir::new("dead-traverse-host");
    let (runtime, mux) = running_host(&host_dir);
    let dead = url::Url::from_file_path(host_dir.0.join("missing").join("relays.json")).unwrap();
    let host = TraverseHost::start(
        mux.clone(),
        HostConfig {
            host_name: "Test Host".into(),
            data_dir: host_dir.0.clone(),
            traverse: vec![ManifestSource::Custom(dead.clone())],
            pairing_enabled: true,
            bind_port: None,
        },
    )
    .expect("hosting starts without the manifest");
    assert_eq!(host.endpoint_id().len(), 64);
    assert!(host.addr().relay.is_none(), "no relay in hand");
    let dir = TestDir::new("dead-traverse-phone");
    let phone = device(&dir, "phone");
    let minted = host.new_invitation();
    assert_eq!(minted.invite.traverse, [dead.to_string()]);
    let paired = pair(&minted.invite, &phone).unwrap();
    assert_eq!(paired.traverse, [dead.to_string()]);
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, syncing_directly);
    client.to_host.send_blocking(subscribe(1)).unwrap();
    recv_type(&client, "ack", Some(1));
    wait_state(&client, connected_directly);
    assert!(
        phone.relays().is_empty(),
        "the device took no relay from anywhere"
    );
    client.to_host.close();
    host.shutdown();
    real_host::stop(runtime, &mux);
}

/// Leaving the app parks its link: the connection closes and so does the
/// device's endpoint, home relay and all, and the machine sees the device
/// gone. The return binds a new endpoint with the same key, dials again
/// and delivers what was written while the app was away.
#[test]
fn a_parked_link_closes_the_endpoint_and_the_return_delivers_what_was_written_meanwhile() {
    let host_dir = TestDir::new("park-host");
    let (runtime, mux) = running_host(&host_dir);
    let host = start_host(mux.clone(), &host_dir, None);
    let dir = TestDir::new("park-device");
    cache_manifest(&dir.0.join("traverse-manifest-official.json"), "official");
    let phone = device(&dir, "phone");
    let minted = host.new_invitation();
    // The test machine runs Off; the saved copy claims the official
    // service, which the machine never checks, so the endpoint carries a relay.
    let paired = PairedHost {
        traverse: Vec::new(),
        ..pair(&minted.invite, &phone).unwrap()
    };
    tcode_traverse::hosts::save_hosts(&dir.0, std::slice::from_ref(&paired)).unwrap();
    phone.hosts_changed();
    wait_relays(&phone, &["https://official.relay.test/"]);
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, syncing_directly);
    client.to_host.send_blocking(subscribe(1)).unwrap();
    recv_type(&client, "ack", Some(1));
    wait_state(&client, connected_directly);

    let hosting = host.hosting_events();
    client.to_host.wake(Wake::Park);
    wait_state(&client, |state| {
        matches!(state, ConnectionState::Reconnecting { .. })
    });
    wait_relays(&phone, &[]);
    while host.devices()[0].live.is_some() {
        assert!(next(&hosting).is_some(), "the machine saw the device leave");
    }
    let open = json!({"id": 2, "payload": {"type": "command", "content": {"type": "open_latest_session"}}});
    client.to_host.send_blocking(open.to_string()).unwrap();
    client.to_host.wake(Wake::Reconnect);
    wait_state(&client, syncing_directly);
    recv_type(&client, "ack", Some(2));
    assert_eq!(phone.relays(), ["https://official.relay.test/"]);
    host.shutdown();
    real_host::stop(runtime, &mux);
}

#[test]
fn two_devices_route_acks_broadcast_events_and_scope_keys() {
    let host_dir = TestDir::new("route-host");
    let (runtime, mux) = running_host(&host_dir);
    let host = start_host(mux.clone(), &host_dir, None);
    let dir_a = TestDir::new("route-a");
    let dir_b = TestDir::new("route-b");
    let device_a = device(&dir_a, "A");
    let device_b = device(&dir_b, "B");
    let minted = host.new_invitation();
    let host_a = pair(&minted.invite, &device_a).unwrap();
    let minted = host.new_invitation();
    let host_b = pair(&minted.invite, &device_b).unwrap();
    let client_a = tcode_traverse::connect(&host_a, &device_a);
    let client_b = tcode_traverse::connect(&host_b, &device_b);
    wait_state(&client_a, syncing_directly);
    wait_state(&client_b, syncing_directly);
    client_a.to_host.send_blocking(subscribe(10)).unwrap();
    client_b.to_host.send_blocking(subscribe(20)).unwrap();
    recv_type(&client_a, "event", None);
    recv_type(&client_b, "event", None);
    recv_type(&client_a, "ack", Some(10));
    recv_type(&client_b, "ack", Some(20));
    wait_state(&client_a, connected_directly);
    wait_state(&client_b, connected_directly);
    let live = host.devices();
    assert!(
        live.iter()
            .all(|device| device.live.as_ref().is_some_and(|live| live.direct)),
        "the connections are direct: {live:?}"
    );

    // The same retained-command key from each device: the host replays a
    // key's first answer, so only a key scoped by the authenticated device
    // lets B's command run on its own.
    let key = "3f2b8c6e-1d4a-4b9e-8c7d-2a1f0e9d8c7b";
    let create = |id: u64, root: &std::path::Path| {
        std::fs::create_dir_all(root).unwrap();
        json!({
            "id": id,
            "key": key,
            "payload": {"type": "command", "content": {"type": "create_project", "content": {"root": root}}}
        })
        .to_string()
    };
    let project = |ack: Value| ack["content"]["result"]["Ok"]["content"].clone();
    client_a
        .to_host
        .send_blocking(create(11, &host_dir.0.join("a")))
        .unwrap();
    let from_a = project(recv_type(&client_a, "ack", Some(11)));
    assert!(from_a.is_string(), "{from_a}");
    client_b
        .to_host
        .send_blocking(create(21, &host_dir.0.join("b")))
        .unwrap();
    // B's own acknowledgment comes after A's on the host stream, so it also
    // bounds the check that nothing addressed to A reached B.
    let from_b = loop {
        let line = next(&client_b.from_host).expect("B's command was acknowledged");
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_ne!(
            value["content"]["id"].as_u64(),
            Some(11),
            "A's acknowledgment leaked to B"
        );
        if value["type"] == "event" {
            assert_eq!(value["content"]["request_id"], Value::Null);
        }
        if value["type"] == "ack" && value["content"]["id"] == 21 {
            break project(value);
        }
    };
    assert!(from_b.is_string(), "{from_b}");
    assert_ne!(
        from_a, from_b,
        "keys are scoped by the authenticated device"
    );
    client_a
        .to_host
        .send_blocking(create(13, &host_dir.0.join("b")))
        .unwrap();
    assert_eq!(
        project(recv_type(&client_a, "ack", Some(13))),
        from_a,
        "A's key is retained: its first answer is replayed"
    );
    // The hosting query is answered by the transport itself.
    client_a
        .to_host
        .send_blocking(
            json!({"id": 12, "payload": {"type":"query","content":{"type":"hosting","content":{"action":{"type":"state"}}}}})
                .to_string(),
        )
        .unwrap();
    let state = recv_type(&client_a, "query_result", Some(12));
    let devices = state["content"]["result"]["Ok"]["content"]["devices"]
        .as_array()
        .unwrap();
    assert_eq!(devices.len(), 2);
    client_a.to_host.close();
    client_b.to_host.close();
    host.shutdown();
    real_host::stop(runtime, &mux);
}

fn follow_hosting(id: u64) -> String {
    json!({"id": id, "payload": {"type": "subscribe", "content": {"topic": {"type": "hosting"}, "after": null}}})
        .to_string()
}

/// Whoever follows a machine's hosting state hears of each change made
/// elsewhere without reading again: this machine's own window through the
/// host, a full device through the topic, which answers with the state as it
/// stands.
#[test]
fn hosting_followers_hear_of_a_change_made_elsewhere() {
    let host_dir = TestDir::new("follow-host");
    let silent = silent_host();
    let host = start_host(silent.mux.clone(), &host_dir, None);
    let window = host.hosting_events();
    let design = host.create_space("Design".into()).unwrap();
    loop {
        let state = next(&window).expect("the window was told");
        if state.spaces.iter().any(|space| space.id == design) {
            break;
        }
    }

    let dir = TestDir::new("follow-phone");
    let phone = device(&dir, "phone");
    let minted = host.new_invitation();
    let paired = pair(&minted.invite, &phone).unwrap();
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, syncing_directly);
    client.to_host.send_blocking(follow_hosting(1)).unwrap();
    let reply = recv_type(&client, "event", None);
    assert_eq!(reply["content"]["request_id"], 1);
    assert_eq!(reply["content"]["topic"], json!({"type": "hosting"}));
    assert_eq!(reply["content"]["event"]["type"], "hosting_replaced");
    assert_eq!(
        reply["content"]["event"]["content"]["spaces"][0]["id"],
        design.as_str()
    );

    let review = host.create_space("Review".into()).unwrap();
    // The device's own path may settle meanwhile, which is a change too.
    loop {
        let pushed = recv_type(&client, "event", None);
        assert_eq!(pushed["content"]["request_id"], Value::Null);
        assert_eq!(pushed["content"]["event"]["type"], "hosting_replaced");
        if pushed["content"]["event"]["content"]["spaces"][1]["id"] == review.as_str() {
            break;
        }
    }
    client.to_host.close();
    host.shutdown();
}

/// A revocation is durable or it did not happen: while the allow list
/// cannot be written, the device stays listed, its connection stays open and
/// it is still admitted, and the caller is told. Once it is written, the
/// live connection closes and a reconnect is refused.
#[test]
fn revocation_closes_the_live_connection_and_rejects_reconnects() {
    let host_dir = TestDir::new("revoke-host");
    let (runtime, mux) = running_host(&host_dir);
    let host = start_host(mux.clone(), &host_dir, None);
    let dir = TestDir::new("revoke-phone");
    let phone = device(&dir, "phone");
    let minted = host.new_invitation();
    let paired = pair(&minted.invite, &phone).unwrap();
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, syncing_directly);
    client.to_host.send_blocking(subscribe(1)).unwrap();
    recv_type(&client, "ack", Some(1));
    assert!(host.devices()[0].live.is_some());

    // The allow list is written through `traverse.tmp`; a directory in its
    // place fails the write on every platform, root or not.
    let blocker = host_dir.0.join("traverse.tmp");
    std::fs::create_dir(&blocker).unwrap();
    let error = host
        .revoke(&phone.endpoint_id().to_string())
        .expect_err("the revocation cannot be recorded");
    assert!(
        matches!(
            host.hosting(tcode_protocol::HostingAction::RevokeDevice(
                phone.endpoint_id().to_string()
            )),
            Err(tcode_protocol::ProtocolError { code, message })
                if code == "revoke_failed" && message.contains(&error.to_string())
        ),
        "a client's revoke is answered with the failure"
    );
    assert_eq!(host.devices().len(), 1, "still paired: {error}");
    assert!(host.devices()[0].live.is_some(), "still connected");
    client.to_host.send_blocking(subscribe(2)).unwrap();
    recv_type(&client, "ack", Some(2));
    let again = tcode_traverse::connect(&paired, &phone);
    wait_state(&again, syncing_directly);
    again.to_host.close();
    std::fs::remove_dir(&blocker).unwrap();

    host.revoke(&phone.endpoint_id().to_string()).unwrap();
    wait_state(&client, |state| {
        state
            == &ConnectionState::Offline {
                reason: ConnectionFailure::AuthenticationRejected,
            }
    });
    assert!(host.devices().is_empty());
    client.to_host.close();
    let again = tcode_traverse::connect(&paired, &phone);
    wait_state(&again, |state| {
        state
            == &ConnectionState::Offline {
                reason: ConnectionFailure::AuthenticationRejected,
            }
    });
    again.to_host.close();
    host.shutdown();
    real_host::stop(runtime, &mux);
}

/// A UDP port the system never hands out to a `:0` bind, free when probed:
/// the app binds its fixed port below every platform's ephemeral range
/// (Linux 32768+, macOS and Windows 49152+), so while a machine is down no
/// other socket on the box is assigned its port behind its back.
fn fixed_port() -> u16 {
    let start = std::process::id() % 10_000;
    (0..10_000)
        .map(|offset| 20_000 + ((start + offset) % 10_000) as u16)
        .find(|&port| std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok())
        .expect("a free UDP port below the ephemeral range")
}

#[test]
fn a_restarted_machine_rejoins_and_delivers_writes_buffered_while_it_was_down() {
    let host_dir = TestDir::new("restart-host");
    let (runtime, mux) = running_host(&host_dir);
    let port = fixed_port();
    let host = start_host(mux.clone(), &host_dir, Some(port));
    let dir = TestDir::new("restart-phone");
    let phone = device(&dir, "phone");
    phone.set_details("phone".into(), Some("Android 15".into()));
    let minted = host.new_invitation();
    let paired = pair(&minted.invite, &phone).unwrap();
    let client = tcode_traverse::connect(&paired, &phone);
    // Drive the same subscription owner as the app, while observing raw replies
    // here so the disconnected command still exercises the native write buffer.
    let link = tcode_client::HostLink::new(client.to_host.clone(), client.from_host.clone());
    wait_state(&client, |state| {
        link.set_connection_state(state.clone());
        syncing_directly(state)
    });
    link.subscribe(tcode_protocol::Subscription {
        topic: tcode_protocol::Topic::Index,
        after: None,
        known_index: None,
    })
    .unwrap();
    recv_type(&client, "event", None);
    wait_state(&client, |state| {
        link.set_connection_state(state.clone());
        connected_directly(state)
    });
    let endpoint_id = host.endpoint_id();

    host.set_pairing_enabled(false);
    host.shutdown();
    wait_state(&client, |state| {
        link.set_connection_state(state.clone());
        matches!(
            state,
            ConnectionState::Reconnecting {
                reason: Some(ConnectionFailure::HostClosed),
                ..
            }
        )
    });
    client
        .to_host
        .send_blocking(
            json!({"id": 0, "payload": {"type":"command", "content":{"type":"open_latest_session"}}})
                .to_string(),
        )
        .unwrap();
    let restarted = TraverseHost::start(
        mux.clone(),
        HostConfig {
            host_name: "Renamed".into(),
            data_dir: host_dir.0.clone(),
            traverse: Vec::new(),
            pairing_enabled: true,
            bind_port: Some(port),
        },
    )
    .unwrap();
    assert_eq!(
        restarted.endpoint_id(),
        endpoint_id,
        "same key, same identity"
    );
    assert!(!restarted.pairing_enabled());
    let identity_path = host_dir.0.join("traverse.json");
    let stored: Value = serde_json::from_slice(&std::fs::read(&identity_path).unwrap()).unwrap();
    assert_eq!(stored["host_name"], "Renamed");
    assert_eq!(stored["pairing_enabled"], false);
    assert_eq!(stored["devices"][0]["id"], phone.endpoint_id().to_string());
    assert_eq!(stored["devices"][0]["platform"], "Android 15");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&identity_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // The rejoin is the replayed subscription's snapshot and the buffered
    // command's ack, both arriving over the new connection.
    wait_state(&client, |state| {
        link.set_connection_state(state.clone());
        connected_directly(state)
    });
    let (mut resubscribed, mut acked) = (false, false);
    while !(resubscribed && acked) {
        let line = next(&client.from_host).unwrap_or_else(|| {
            panic!("rejoin incomplete: resubscribed {resubscribed}, buffered write acked {acked}")
        });
        let value: Value = serde_json::from_str(line.trim_end()).unwrap();
        resubscribed |= value["type"] == "event" && !value["content"]["request_id"].is_null();
        acked |= value["type"] == "ack" && value["content"]["id"] == 0;
    }
    link.close();
    restarted.shutdown();
    real_host::stop(runtime, &mux);
}

fn space_invite(host: &TraverseHost, id: &str) -> PairInvite {
    tcode_client::pairing::parse_pair_url(&host.space_link_url(id).unwrap()).unwrap()
}

fn scoped_ping(
    client: &Transport,
    host_rx: &async_channel::Receiver<tcode_client::attachment::AttachmentMessage>,
    host_tx: &async_channel::Sender<String>,
    principal: &tcode_protocol::Principal,
    id: u64,
) {
    let key = format!("00000000-0000-4000-8000-{id:012x}");
    let tcode_protocol::Principal::Space { device_id, .. } = principal else {
        panic!("scoped ping requires a space member");
    };
    let scoped_key = format!("{device_id}:{key}");
    client
        .to_host
        .send_blocking(
            tcode_protocol::encode_line(&tcode_protocol::ClientMessage {
                id,
                key: Some(key),
                principal: Some(tcode_protocol::Principal::Full),
                payload: tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Ping),
            })
            .unwrap(),
        )
        .unwrap();
    // Other connections can send idle heartbeats into this shared host queue.
    // Mux request IDs are global, so identify the probe by its retained key.
    tcode_traverse::block_on(async {
        {
            loop {
                let line = host_rx.recv().await.unwrap().line;
                let request = tcode_protocol::decode_client_line(&line).unwrap();
                assert_eq!(
                    request.payload,
                    tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Ping)
                );
                if let Some(key) = &request.key {
                    assert_eq!(key, &scoped_key);
                    assert_eq!(request.principal.as_ref(), Some(principal));
                }
                host_tx
                    .send(
                        tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                            id: request.id,
                            result: Ok(tcode_protocol::QueryResponse::Pong),
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                if request.key.is_some() {
                    break;
                }
            }
        }
    });
    let reply = recv_type(client, "query_result", Some(id));
    assert_eq!(reply["content"]["result"]["Ok"]["type"], "pong");
}

/// A member dropped by a change to its grant comes back with the new one.
/// The device's return to the app wakes the link, as the shell sends it,
/// rather than the backoff timer, whose length is not what these tests are
/// about.
fn rejoin(client: &Transport) {
    wait_state(client, |state| {
        matches!(state, ConnectionState::Reconnecting { .. })
    });
    client.to_host.wake(Wake::Reconnect);
    wait_state(client, syncing_directly);
}

fn assert_stayed_connected(client: &Transport) {
    while let Ok(state) = client.state.try_recv() {
        assert!(
            matches!(
                state,
                ConnectionState::Syncing { .. } | ConnectionState::Connected { .. }
            ),
            "member connection changed: {state:?}"
        );
    }
}

#[test]
fn space_links_scope_members_survive_link_changes_and_revoke_until_repaired() {
    use tcode_protocol::{DeviceAccess, HostingAction, Principal, SpaceAction};
    use tcode_traverse::identity::{DeviceGrant, HostIdentity};

    let host_dir = TestDir::new("spaces-host");
    let (to_host, host_rx) = async_channel::unbounded();
    let (host_tx, from_host) = async_channel::unbounded();
    let host = start_host(HostMux::new(to_host, from_host), &host_dir, None);
    host.new_invitation();
    let created = host
        .hosting(HostingAction::Spaces(SpaceAction::Create {
            name: "Shared".into(),
        }))
        .unwrap();
    let id = created.created_space_id.unwrap();
    assert_eq!(created.spaces[0].id, id);
    assert!(
        host.hosting(HostingAction::State)
            .unwrap()
            .created_space_id
            .is_none()
    );
    host.set_space_projects(&id, vec!["project-a".into(), "project-b".into()])
        .unwrap();
    let invite = space_invite(&host, &id);
    assert_eq!(invite.space.as_deref(), Some(id.as_str()));
    let member_dir = TestDir::new("spaces-member");
    let member = device(&member_dir, "Collaborator");
    let other_dir = TestDir::new("spaces-other");
    let other = device(&other_dir, "Other");
    let paired = pair(&invite, &member).unwrap();
    assert!(host.invitation().is_some());
    assert_eq!(paired.space_id.as_deref(), Some(id.as_str()));
    assert_eq!(paired.space_name.as_deref(), Some("Shared"));
    tcode_traverse::hosts::save_hosts(&member_dir.0, std::slice::from_ref(&paired)).unwrap();
    let saved = tcode_traverse::hosts::load_hosts(&member_dir.0).unwrap();
    assert_eq!(saved[0].space_id.as_deref(), Some(id.as_str()));
    assert_eq!(saved[0].space_name.as_deref(), Some("Shared"));
    let persisted = HostIdentity::load_or_create(&host_dir.0, "Test Host").unwrap();
    assert_eq!(
        persisted.devices[0].access,
        DeviceGrant::Spaces(vec![id.clone()])
    );
    let client = tcode_traverse::connect(&paired, &member);
    wait_state(&client, syncing_directly);
    let mut principal = Principal::Space {
        policy_revision: persisted.policy_revision,
        space_id: id.clone(),
        space_name: "Shared".into(),
        project_ids: vec!["project-a".into(), "project-b".into()],
        device_id: member.endpoint_id().to_string(),
        device_name: "Collaborator".into(),
        policy: tcode_protocol::SpacePolicy::new_space(),
    };
    scoped_ping(&client, &host_rx, &host_tx, &principal, 1);
    let state = host.hosting(HostingAction::State).unwrap();
    assert_eq!(
        state.devices[0].access,
        DeviceAccess::Space {
            space_id: id.clone()
        }
    );
    assert!(state.spaces[0].members[0].path.is_some());
    assert_eq!(pair(&invite, &member), Err(PairError::AlreadyMember));

    client
        .to_host
        .send_blocking(
            tcode_protocol::encode_line(&tcode_protocol::ClientMessage {
                id: 2,
                key: None,
                principal: Some(Principal::Full),
                payload: tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Hosting {
                    action: HostingAction::Spaces(SpaceAction::Create {
                        name: "forged".into(),
                    }),
                }),
            })
            .unwrap(),
        )
        .unwrap();
    let refusal = recv_type(&client, "query_result", Some(2));
    assert_eq!(refusal["content"]["result"]["Err"]["code"], "out_of_scope");
    assert_eq!(host.spaces().len(), 1);
    client.to_host.send_blocking(follow_hosting(30)).unwrap();
    let refusal = recv_type(&client, "ack", Some(30));
    assert_eq!(refusal["content"]["result"]["Err"]["code"], "out_of_scope");
    let opener = client.current_host.as_ref().unwrap().tunnels().unwrap();
    let refusal = tcode_traverse::block_on(opener.open(StreamTarget::Tcp {
        host: "127.0.0.1".into(),
        port: 1,
    }))
    .unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("this space does not allow Preview tunnels"),
        "{refusal}"
    );
    let refusal = tcode_traverse::block_on(opener.open(StreamTarget::Ssh)).unwrap_err();
    assert_eq!(refusal.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        refusal
            .to_string()
            .contains("this space does not allow opening its projects in an editor"),
        "{refusal}"
    );

    host.set_space_link_enabled(&id, false).unwrap();
    assert!(host.space_link_url(&id).is_none());
    assert_eq!(pair(&invite, &other), Err(PairError::SpaceUnavailable));
    host.set_space_link_enabled(&id, true).unwrap();
    host.set_pairing_enabled(false);
    assert_eq!(pair(&invite, &other), Err(PairError::Disabled));
    host.set_pairing_enabled(true);
    let one_shot = host.new_invitation();
    let wrong = PairInvite {
        secret: "AAAAAAAAAAAAAAAAAAAAAA".into(),
        ..invite.clone()
    };
    let blocker = host_dir.0.join("traverse.tmp");
    std::fs::create_dir(&blocker).unwrap();
    assert_eq!(pair(&wrong, &other), Err(PairError::Busy));
    assert_eq!(pair(&invite, &other), Err(PairError::Busy));
    assert_eq!(host.devices().len(), 1);
    assert_eq!(
        HostIdentity::load_or_create(&host_dir.0, "Test Host")
            .unwrap()
            .spaces[0]
            .link_failures,
        0
    );
    std::fs::remove_dir(blocker).unwrap();
    for _ in 0..5 {
        assert_eq!(pair(&wrong, &other), Err(PairError::Invalid));
    }
    assert!(host.spaces()[0].link_dead);
    assert!(host.space_link_url(&id).is_none());
    assert_eq!(
        HostIdentity::load_or_create(&host_dir.0, "Test Host")
            .unwrap()
            .spaces[0]
            .link_failures,
        5
    );
    assert_eq!(pair(&invite, &other), Err(PairError::SpaceUnavailable));
    let full_dir = TestDir::new("spaces-full");
    let full = device(&full_dir, "Owner");
    let full_paired = pair(&one_shot.invite, &full).unwrap();
    assert!(full_paired.space_id.is_none());
    scoped_ping(&client, &host_rx, &host_tx, &principal, 3);
    assert_stayed_connected(&client);

    host.regenerate_space_link(&id).unwrap();
    let regenerated = space_invite(&host, &id);
    assert_eq!(pair(&regenerated, &full), Err(PairError::AlreadyMember));

    assert_ne!(regenerated.secret, invite.secret);
    assert_eq!(pair(&invite, &other), Err(PairError::Invalid));
    let other_paired = pair(&regenerated, &other).unwrap();
    let other_client = tcode_traverse::connect(&other_paired, &other);
    wait_state(&other_client, syncing_directly);
    let mut other_principal = principal.clone();
    if let Principal::Space {
        policy_revision, ..
    } = &mut other_principal
    {
        *policy_revision = HostIdentity::load_or_create(&host_dir.0, "Test Host")
            .unwrap()
            .policy_revision;
    }
    if let Principal::Space {
        device_id,
        device_name,
        ..
    } = &mut other_principal
    {
        *device_id = other.endpoint_id().to_string();
        *device_name = "Other".into();
    }
    scoped_ping(&other_client, &host_rx, &host_tx, &other_principal, 10);
    scoped_ping(&client, &host_rx, &host_tx, &principal, 4);
    assert_stayed_connected(&client);

    host.remove_member(&member.endpoint_id().to_string(), false)
        .unwrap();
    let unpaired = |state: &ConnectionState| {
        state
            == &ConnectionState::Offline {
                reason: ConnectionFailure::AuthenticationRejected,
            }
    };
    wait_state(&client, unpaired);
    client.to_host.close();
    let rejected = tcode_traverse::connect(&paired, &member);
    wait_state(&rejected, unpaired);
    rejected.to_host.close();
    pair(&regenerated, &member).unwrap();
    let blocker = host_dir.0.join("traverse.tmp");
    std::fs::create_dir(&blocker).unwrap();
    assert!(
        host.remove_member(&member.endpoint_id().to_string(), true)
            .is_err()
    );
    assert_eq!(space_invite(&host, &id).secret, regenerated.secret);
    assert!(
        host.devices()
            .iter()
            .any(|device| device.id == member.endpoint_id().to_string())
    );
    std::fs::remove_dir(blocker).unwrap();
    host.hosting(HostingAction::Spaces(SpaceAction::RemoveMember {
        device_id: member.endpoint_id().to_string(),
        regenerate_link: true,
    }))
    .unwrap();
    assert_eq!(pair(&regenerated, &member), Err(PairError::Invalid));
    let repaired = pair(&space_invite(&host, &id), &member).unwrap();
    if let Principal::Space {
        policy_revision, ..
    } = &mut principal
    {
        *policy_revision = HostIdentity::load_or_create(&host_dir.0, "Test Host")
            .unwrap()
            .policy_revision;
    }
    let client = tcode_traverse::connect(&repaired, &member);
    wait_state(&client, syncing_directly);
    scoped_ping(&client, &host_rx, &host_tx, &principal, 5);

    let blocker = host_dir.0.join("traverse.tmp");
    std::fs::create_dir(&blocker).unwrap();
    assert!(
        host.set_space_projects(&id, vec!["unpersisted".into()])
            .is_err()
    );
    assert_eq!(host.spaces()[0].project_ids, ["project-a", "project-b"]);
    scoped_ping(&client, &host_rx, &host_tx, &principal, 6);
    std::fs::remove_dir(blocker).unwrap();
    host.hosting(HostingAction::Spaces(SpaceAction::SetProjects {
        id: id.clone(),
        project_ids: vec!["project-b".into()],
    }))
    .unwrap();
    rejoin(&client);
    if let Principal::Space {
        project_ids,
        policy_revision,
        ..
    } = &mut principal
    {
        *policy_revision = HostIdentity::load_or_create(&host_dir.0, "Test Host")
            .unwrap()
            .policy_revision;
        *project_ids = vec!["project-b".into()];
    }
    scoped_ping(&client, &host_rx, &host_tx, &principal, 7);
    rejoin(&other_client);
    if let Principal::Space {
        project_ids,
        policy_revision,
        ..
    } = &mut other_principal
    {
        *policy_revision = HostIdentity::load_or_create(&host_dir.0, "Test Host")
            .unwrap()
            .policy_revision;
        *project_ids = vec!["project-b".into()];
    }
    scoped_ping(&other_client, &host_rx, &host_tx, &other_principal, 11);
    host.rename_space(&id, "Renamed".into()).unwrap();
    rejoin(&client);
    if let Principal::Space {
        space_name,
        policy_revision,
        ..
    } = &mut principal
    {
        *policy_revision = HostIdentity::load_or_create(&host_dir.0, "Test Host")
            .unwrap()
            .policy_revision;
        *space_name = "Renamed".into();
    }
    scoped_ping(&client, &host_rx, &host_tx, &principal, 8);
    let moved_space = host.create_space("Moved".into()).unwrap();
    let moved_invite = space_invite(&host, &moved_space);
    assert_eq!(pair(&moved_invite, &member), Err(PairError::AlreadyMember));
    host.move_member(&member.endpoint_id().to_string(), &moved_space)
        .unwrap();
    rejoin(&client);
    if let Principal::Space {
        space_id,
        space_name,
        project_ids,
        policy_revision,
        ..
    } = &mut principal
    {
        *policy_revision = HostIdentity::load_or_create(&host_dir.0, "Test Host")
            .unwrap()
            .policy_revision;
        *space_id = moved_space.clone();
        *space_name = "Moved".into();
        project_ids.clear();
    }
    scoped_ping(&client, &host_rx, &host_tx, &principal, 9);
    host.delete_space(&id).unwrap();
    wait_state(&other_client, unpaired);
    other_client.to_host.close();
    scoped_ping(&client, &host_rx, &host_tx, &principal, 12);
    assert_stayed_connected(&client);
    host.delete_space(&moved_space).unwrap();

    wait_state(&client, unpaired);
    assert_eq!(host.devices().len(), 1);
    assert_eq!(host.devices()[0].access, DeviceAccess::Full);
    assert_eq!(
        pair(&regenerated, &member),
        Err(PairError::SpaceUnavailable)
    );
    client.to_host.close();
    host.shutdown();
}

#[test]
fn legacy_devices_migrate_to_full_and_v3_missing_or_invalid_access_fails_closed() {
    use tcode_traverse::identity::{DeviceGrant, HOST_FILE, HostIdentity};
    let dir = TestDir::new("spaces-migration");
    let key = iroh::SecretKey::generate();
    let key_hex: String = key
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let legacy = format!(
        r#"{{"v":2,"host_name":"Test Host","secret_key":"{key_hex}","devices":[{{"id":"{}","name":"Legacy","platform":"Android","created_unix":42}}],"pairing_enabled":false}}"#,
        iroh::SecretKey::generate().public()
    );
    std::fs::write(dir.0.join(HOST_FILE), legacy).unwrap();
    let identity = HostIdentity::load_or_create(&dir.0, "Test Host").unwrap();
    assert_eq!(identity.endpoint_id(), key.public());
    assert_eq!(identity.devices[0].access, DeviceGrant::Full);
    assert!(identity.spaces.is_empty());
    assert!(!identity.pairing_enabled);
    let migrated = std::fs::read(dir.0.join(HOST_FILE)).unwrap();
    let mut persisted: Value = serde_json::from_slice(&migrated).unwrap();
    assert_eq!(identity.policy_revision, 0);
    // A version 3 file, as releases before groups wrote it, that had no
    // policy revision yet loads with no group.
    persisted["v"] = 3.into();
    let mut older_v3 = persisted.clone();
    older_v3.as_object_mut().unwrap().remove("policy_revision");
    std::fs::write(
        dir.0.join(HOST_FILE),
        serde_json::to_vec(&older_v3).unwrap(),
    )
    .unwrap();
    let v3 = HostIdentity::load_or_create(&dir.0, "Test Host").unwrap();
    assert_eq!(v3.policy_revision, 0);
    assert!(v3.group.is_none());
    // A space stored before spaces had a policy keeps what its members could
    // do then.
    let mut spaced = persisted.clone();
    spaced["spaces"] = json!([{"id":"s","name":"Shared","created_unix":1,"project_ids":["p"],
        "secret":"AAAAAAAAAAAAAAAAAAAAAA","link_enabled":true,"link_failures":0}]);
    std::fs::write(dir.0.join(HOST_FILE), serde_json::to_vec(&spaced).unwrap()).unwrap();
    let mut spaced = HostIdentity::load_or_create(&dir.0, "Test Host").unwrap();
    assert_eq!(
        spaced.spaces[0].policy,
        tcode_protocol::SpacePolicy::unrestricted()
    );
    spaced.spaces[0].policy = tcode_protocol::SpacePolicy::new_space();
    spaced.save().unwrap();
    assert_eq!(
        HostIdentity::load_or_create(&dir.0, "Test Host")
            .unwrap()
            .spaces[0]
            .policy,
        tcode_protocol::SpacePolicy::new_space()
    );
    assert_eq!(persisted["devices"][0]["access"], json!({"type":"full"}));
    for access in [
        None,
        Some(json!({"type":"unknown"})),
        Some(json!({"type":"spaces","content":"wrong"})),
    ] {
        let mut invalid = persisted.clone();
        let record = invalid["devices"][0].as_object_mut().unwrap();
        record.remove("access");
        if let Some(access) = access {
            record.insert("access".into(), access);
        }
        let bytes = serde_json::to_vec(&invalid).unwrap();
        std::fs::write(dir.0.join(HOST_FILE), &bytes).unwrap();
        assert!(HostIdentity::load_or_create(&dir.0, "Test Host").is_err());
        assert_eq!(std::fs::read(dir.0.join(HOST_FILE)).unwrap(), bytes);
    }
}

/// Several times QUIC's 1.25 MB per-stream receive window (noq-proto 1.4
/// `TransportConfig::default`), so a transfer stays open on the machine
/// until the device reads or cancels it.
const ORIGINAL_BYTES: usize = 4 * 1024 * 1024;

/// Contract: a large original and a recorded image leave control responsive;
/// cancellation releases admission even when both device slots are occupied.
#[test]
fn originals_preserve_control_progress_bytes_and_cancelled_admission() {
    use tcode_client::HostLink;
    use tcode_protocol::{OriginalTarget, Query, QueryResponse};
    let host_dir = TestDir::new("originals");
    let device_dir = TestDir::new("original-device");
    let bytes: Vec<_> = (0..ORIGINAL_BYTES)
        .map(|index| ((index * 17 + index / 257) % 251) as u8)
        .collect();
    let path = host_dir.0.join("original.png");
    std::fs::write(&path, &bytes).unwrap();
    let (runtime, mux, session_id, _) = original_fixture::recorded_host(&host_dir.0, &bytes);
    let host = start_host(mux.clone(), &host_dir, None);
    let device = device(&device_dir, "Original phone");
    let paired = pair(&host.new_invitation().invite, &device).unwrap();
    let transport = tcode_traverse::connect(&paired, &device);
    wait_state(&transport, syncing_directly);
    let link = HostLink::new_remote(transport.to_host, transport.from_host, transport.originals);
    tcode_traverse::block_on(async {
        let pump = tcode_traverse::runtime().spawn({
            let link = link.clone();
            async move { link.pump().await }
        });
        let target = OriginalTarget::File { path: path.clone() };
        let mut file = link.read_original(target.clone());
        assert_eq!(file.metadata().await.unwrap().len, bytes.len() as u64);
        let mut received = file.read().await.unwrap().unwrap();
        assert_eq!(link.query(Query::Ping).await.unwrap(), QueryResponse::Pong);
        while let Some(block) = file.read().await.unwrap() {
            received.extend(block);
        }
        assert_eq!(received, bytes);
        drop(file);
        let mut item = link.read_original(OriginalTarget::Item {
            session_id,
            item_id: "recorded-image".into(),
            image_index: 0,
        });
        assert_eq!(item.metadata().await.unwrap().len, bytes.len() as u64);
        let mut received = item.read().await.unwrap().unwrap();
        assert_eq!(link.query(Query::Ping).await.unwrap(), QueryResponse::Pong);
        while let Some(block) = item.read().await.unwrap() {
            received.extend(block);
        }
        assert_eq!(received, bytes);
        drop(item);
        let mut first = admitted_original(&link, &target).await;
        first.read().await.unwrap().unwrap();
        let mut second = admitted_original(&link, &target).await;
        second.read().await.unwrap().unwrap();
        let mut refused = link.read_original(target.clone());
        assert_eq!(refused.metadata().await.unwrap_err().code, "busy");
        first.cancel();
        assert!(first.read().await.is_err());
        assert_eq!(link.query(Query::Ping).await.unwrap(), QueryResponse::Pong);
        let mut replacement = admitted_original(&link, &target).await;
        let mut received = Vec::new();
        while let Some(block) = replacement.read().await.unwrap() {
            received.extend(block);
        }
        assert_eq!(received, bytes);
        drop(second);
        link.close();
        pump.await.unwrap();
    });
    host.shutdown();
    real_host::stop(runtime, &mux);
}

/// A paired device's upload crosses the real connection in protocol-sized
/// chunks and arrives whole in the host's scratch directory.
#[test]
fn a_device_uploads_a_file_to_the_hosts_scratch_directory() {
    use tcode_client::HostLink;
    use tcode_protocol::{Query, QueryResponse};
    let host_dir = TestDir::new("upload");
    let device_dir = TestDir::new("upload-device");
    let documents = host_dir.0.join("documents");
    let (runtime, mux, session_id, _) = original_fixture::recorded_host_with(
        &host_dir.0,
        b"recorded image",
        tcode_runtime::pipe::HostServices {
            user_directories: tcode_services::user_files::UserDirectories {
                home: None,
                documents: Some(documents.clone()),
            },
            ..Default::default()
        },
    );
    let host = start_host(mux.clone(), &host_dir, None);
    let device = device(&device_dir, "Upload phone");
    let paired = pair(&host.new_invitation().invite, &device).unwrap();
    let transport = tcode_traverse::connect(&paired, &device);
    wait_state(&transport, syncing_directly);
    let link = HostLink::new_remote(transport.to_host, transport.from_host, transport.originals);
    let content: Vec<_> = (0..tcode_core::attachments::UPLOAD_CHUNK_BYTES * 3 + 11)
        .map(|index| (index % 251) as u8)
        .collect();
    tcode_traverse::block_on(async {
        let pump = tcode_traverse::runtime().spawn({
            let link = link.clone();
            async move { link.pump().await }
        });
        let QueryResponse::UploadBegun(path) = link
            .query(Query::BeginUpload {
                session_id: session_id.clone(),
                name: "report.pdf".into(),
                size: content.len() as u64,
            })
            .await
            .unwrap()
        else {
            panic!("upload refused");
        };
        assert!(path.starts_with(documents.join("Tcode")));
        assert_eq!(path.file_name().unwrap(), "report.pdf");
        let mut offset = 0;
        for chunk in content.chunks(tcode_core::attachments::UPLOAD_CHUNK_BYTES) {
            assert_eq!(
                link.query(Query::WriteUpload {
                    session_id: session_id.clone(),
                    path: path.clone(),
                    offset,
                    bytes: chunk.to_vec(),
                })
                .await
                .unwrap(),
                QueryResponse::UploadWritten
            );
            offset += chunk.len() as u64;
        }
        assert_eq!(std::fs::read(&path).unwrap(), content);
        link.close();
        pump.await.unwrap();
    });
    host.shutdown();
    real_host::stop(runtime, &mux);
}

/// Opens `target` once the host admits it. A read gives its slot back when the
/// host sees the client stop the stream, which can trail the client reading
/// the end or cancelling.
async fn admitted_original(
    link: &tcode_client::HostLink,
    target: &tcode_protocol::OriginalTarget,
) -> tcode_client::OriginalTransfer {
    loop {
        let mut next = link.read_original(target.clone());
        match next.metadata().await {
            Ok(_) => break next,
            Err(error) if error.code == "busy" => tokio::task::yield_now().await,
            Err(error) => panic!("original: {error:?}"),
        }
    }
}

/// Contract: revoking a project grant prevents a blocked original from completing.
#[test]
fn revoked_project_original_never_completes() {
    use tcode_client::HostLink;
    use tcode_protocol::{HostingAction, OriginalTarget, SpaceAction};
    let host_dir = TestDir::new("original-revoke");
    let device_dir = TestDir::new("original-member");
    let bytes = vec![0x91; ORIGINAL_BYTES];
    let path = host_dir.0.join("original.png");
    std::fs::write(&path, &bytes).unwrap();
    let (runtime, mux, _, project_id) =
        original_fixture::recorded_host(&host_dir.0, b"recorded image");
    let host = start_host(mux.clone(), &host_dir, None);
    let created = host
        .hosting(HostingAction::Spaces(SpaceAction::Create {
            name: "Original space".into(),
        }))
        .unwrap();
    let space_id = created.created_space_id.unwrap();
    host.set_space_projects(&space_id, vec![project_id])
        .unwrap();
    let device = device(&device_dir, "Member phone");
    let paired = pair(&space_invite(&host, &space_id), &device).unwrap();
    let transport = tcode_traverse::connect(&paired, &device);
    wait_state(&transport, syncing_directly);
    let link = HostLink::new_remote(transport.to_host, transport.from_host, transport.originals);
    tcode_traverse::block_on(async {
        let pump = tcode_traverse::runtime().spawn({
            let link = link.clone();
            async move { link.pump().await }
        });
        let target = OriginalTarget::File { path };
        let mut transfer = link.read_original(target.clone());
        assert!(!transfer.read().await.unwrap().unwrap().is_empty());
        host.set_space_projects(&space_id, Vec::new()).unwrap();
        // The grant change ends the original.
        let result = loop {
            match transfer.read().await {
                Ok(Some(_)) => {}
                other => break other,
            }
        };
        assert!(result.is_err(), "revoked original completed");
        drop(transfer);
        let mut reconnecting = false;
        loop {
            let state = transport.state.recv().await.unwrap();
            if matches!(state, ConnectionState::Reconnecting { .. }) {
                reconnecting = true;
            }
            if reconnecting && matches!(state, ConnectionState::Syncing { .. }) {
                break;
            }
        }
        let mut denied = link.read_original(target);
        assert_eq!(denied.metadata().await.unwrap_err().code, "out_of_scope");
        link.close();
        pump.await.unwrap();
    });
    host.shutdown();
    real_host::stop(runtime, &mux);
}
