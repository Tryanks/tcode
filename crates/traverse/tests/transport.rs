//! Two in-process endpoints over loopback: no relay, no discovery, no network.
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tcode_client::heartbeat::NATIVE_IDLE_MS;
use tcode_client::host::Transport;
use tcode_client::pairing::PairInvite;
use tcode_client::{ConnectionFailure, ConnectionState};
use tcode_traverse::{DeviceIdentity, HostConfig, HostMux, PairError, TraverseHost, TraverseMode};

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

/// Answers subscriptions with an index event, `create_project` with a
/// broadcast event, pings with pong, and acks every command. Records every
/// dedup key it sees.
fn fake_host() -> (
    HostMux,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let (to_host, host_rx) = async_channel::unbounded::<String>();
    let (host_tx, from_host) = async_channel::unbounded::<String>();
    let subscribe_count = Arc::new(AtomicUsize::new(0));
    let keys = Arc::new(std::sync::Mutex::new(Vec::new()));
    let count = subscribe_count.clone();
    let seen_keys = keys.clone();
    std::thread::spawn(move || {
        let event = json!({
            "type": "event",
            "content": {
                "topic": "index",
                "event": {"type": "index_snapshot", "content": {"sessions": [], "projects": []}}
            }
        })
        .to_string();
        while let Ok(line) = host_rx.recv_blocking() {
            let value: Value = serde_json::from_str(line.trim_end()).unwrap();
            let id = value["id"].as_u64().unwrap();
            if let Some(key) = value["key"].as_str() {
                seen_keys.lock().unwrap().push(key.to_owned());
            }
            let kind = value["payload"]["type"].as_str().unwrap();
            if kind == "subscribe" {
                count.fetch_add(1, Ordering::Relaxed);
                host_tx.send_blocking(event.clone()).unwrap();
            }
            if kind == "query" && value["payload"]["content"]["type"] == "ping" {
                host_tx
                    .send_blocking(
                        json!({"type":"query_result","content":{"id":id,"result":{"Ok":{"type":"pong"}}}})
                            .to_string(),
                    )
                    .unwrap();
                continue;
            }
            if kind == "command"
                && value["payload"]["content"]["type"].as_str() == Some("create_project")
            {
                host_tx.send_blocking(event.clone()).unwrap();
            }
            host_tx
                .send_blocking(
                    json!({"type": "ack", "content": {"id": id, "result": {"Ok": {"type": "unit"}}}})
                        .to_string(),
                )
                .unwrap();
        }
    });
    (HostMux::new(to_host, from_host), subscribe_count, keys)
}

fn start_host(mux: HostMux, dir: &TestDir, bind_port: Option<u16>) -> TraverseHost {
    TraverseHost::start(
        mux,
        HostConfig {
            host_name: "Test Host".into(),
            data_dir: dir.0.clone(),
            traverse: TraverseMode::Off,
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

/// Loopback links are carried directly, and the state says so.
fn connected_directly() -> ConnectionState {
    ConnectionState::Connected {
        path: Some(tcode_protocol::PathInfo {
            direct: true,
            relay: None,
            lan: true,
            probing_direct: false,
        }),
    }
}

fn wait_state(transport: &Transport, wanted: ConnectionState) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Ok(state) = transport.state.try_recv() {
            if state == wanted {
                return;
            }
            seen.push(state);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("did not observe state {wanted:?}; saw {seen:?}");
}

fn recv_type(transport: &Transport, kind: &str, id: Option<u64>) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(line) = transport.from_host.try_recv() {
            let value: Value = serde_json::from_str(line.trim_end()).unwrap();
            if value["type"] == kind
                && id.is_none_or(|id| value["content"]["id"].as_u64() == Some(id))
            {
                return value;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("did not receive {kind}");
}

fn subscribe(id: u64) -> String {
    json!({"id": id, "payload": {"type": "subscribe", "content": {"topic": "index"}}}).to_string()
}

#[test]
fn invitations_are_single_use_five_wrong_secrets_invalidate_and_unpaired_devices_are_rejected() {
    let host_dir = TestDir::new("pair-host");
    let (mux, _, _) = fake_host();
    let host = start_host(mux, &host_dir, None);
    let phone_dir = TestDir::new("pair-phone");
    let phone = device(&phone_dir, "phone");
    let other_dir = TestDir::new("pair-other");
    let other = device(&other_dir, "other");

    let minted = host.new_invitation();
    let invite = &minted.invite;
    assert_eq!(invite.host_id, host.endpoint_id());
    assert!(
        invite
            .addrs
            .iter()
            .any(|addr| addr.starts_with("127.0.0.1:")),
        "the invite names the loopback socket: {:?}",
        invite.addrs
    );
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
        assert_eq!(
            tcode_traverse::pair_blocking(&wrong, &phone),
            Err(PairError::Invalid)
        );
    }
    assert_eq!(
        tcode_traverse::pair_blocking(invite, &phone),
        Err(PairError::Invalid),
        "five wrong secrets burn the invitation even for the right one"
    );
    assert!(host.devices().is_empty());
    assert!(host.invitation().is_none());

    let old = host.new_invitation();
    let minted = host.new_invitation();
    assert_ne!(old.invite.secret, minted.invite.secret);
    assert_eq!(
        tcode_traverse::pair_blocking(&old.invite, &phone),
        Err(PairError::Invalid),
        "a new invitation replaces the old one"
    );
    assert_eq!(
        host.invitation().map(|(active, _)| active.invite),
        Some(minted.invite.clone())
    );
    let paired = tcode_traverse::pair_blocking(&minted.invite, &phone).unwrap();
    assert_eq!(paired.host_id, host.endpoint_id());
    assert_eq!(paired.name, "Test Host");
    assert_eq!(paired.addrs, minted.invite.addrs);
    assert_eq!(
        tcode_traverse::pair_blocking(&minted.invite, &other),
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
    wait_state(
        &stranger,
        ConnectionState::Offline {
            reason: ConnectionFailure::AuthenticationRejected,
        },
    );
    stranger.to_host.close();

    host.set_pairing_enabled(false);
    let minted = host.new_invitation();
    assert_eq!(
        tcode_traverse::pair_blocking(&minted.invite, &other),
        Err(PairError::Disabled)
    );
    host.shutdown();
}

/// Whoever keeps a copy of the invitation hears every change, in order:
/// each mint, the burn after five wrong secrets, its use and pairing being
/// turned off. A listener added later hears only what follows.
#[test]
fn invitation_changes_are_reported_in_order() {
    let host_dir = TestDir::new("events-host");
    let (mux, _, _) = fake_host();
    let host = start_host(mux, &host_dir, None);
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
        assert_eq!(
            tcode_traverse::pair_blocking(&wrong, &phone),
            Err(PairError::Invalid)
        );
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
    tcode_traverse::pair_blocking(&third.invite, &phone).unwrap();
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

fn wait_relays(device: &DeviceIdentity, wanted: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = device.relays();
    while seen != wanted && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        seen = device.relays();
    }
    assert_eq!(seen, wanted);
}

/// A device's relays are those of its machines' Traverse instances: a
/// device whose only machine is Off carries none, a machine on the official
/// service brings the official relays, and removing the last such machine
/// takes them away again. Nothing is contacted for this: the official
/// manifest is a cache newer than the bundle, fetched just now.
#[test]
fn device_relays_follow_the_traverse_instances_of_its_machines() {
    let off_dir = TestDir::new("relays-off-host");
    let (mux, _, _) = fake_host();
    let off_host = start_host(mux, &off_dir, None);
    let official_dir = TestDir::new("relays-official-host");
    let (mux, _, _) = fake_host();
    let official_host = start_host(mux, &official_dir, None);
    let dir = TestDir::new("relays-phone");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::write(
        dir.0.join("traverse-manifest-official.json"),
        json!({
            "fetchedAtMs": now_ms,
            "manifest": {
                "version": 1,
                "updatedAt": "2999-01-01T00:00:00Z",
                "relays": [{"url": "https://official.relay.test/"}],
                "pkarr": ["https://official.lookup.test/pkarr"]
            }
        })
        .to_string(),
    )
    .unwrap();
    let phone = device(&dir, "phone");

    let minted = off_host.new_invitation();
    assert_eq!(
        minted.invite.traverse.as_deref(),
        Some(tcode_client::pairing::TRAVERSE_OFF),
        "an Off machine says so in its invitation"
    );
    let off = tcode_traverse::pair_blocking(&minted.invite, &phone).unwrap();
    tcode_traverse::hosts::save_hosts(&dir.0, std::slice::from_ref(&off)).unwrap();
    phone.hosts_changed();
    wait_relays(&phone, &[]);

    // The test machine runs Off; its invitation is rewritten to claim the
    // official service, which the machine never checks.
    let minted = official_host.new_invitation();
    let invite = PairInvite {
        traverse: None,
        ..minted.invite
    };
    let official = tcode_traverse::pair_blocking(&invite, &phone).unwrap();
    assert_eq!(official.traverse, None);
    wait_relays(&phone, &["https://official.relay.test/"]);
    tcode_traverse::hosts::save_hosts(&dir.0, &[off.clone(), official]).unwrap();
    phone.hosts_changed();
    // Reconciling runs in the background; give it time to get it wrong.
    std::thread::sleep(Duration::from_millis(200));
    wait_relays(&phone, &["https://official.relay.test/"]);

    tcode_traverse::hosts::save_hosts(&dir.0, &[off]).unwrap();
    phone.hosts_changed();
    wait_relays(&phone, &[]);

    // A pairing that fails leaves nothing of the instance it tried behind.
    let minted = official_host.new_invitation();
    let wrong = PairInvite {
        traverse: None,
        secret: "AAAAAAAAAAAAAAAAAAAAAA".into(),
        ..minted.invite
    };
    assert_eq!(
        tcode_traverse::pair_blocking(&wrong, &phone),
        Err(PairError::Invalid)
    );
    wait_relays(&phone, &[]);
    off_host.shutdown();
    official_host.shutdown();
}

/// A self-hosted instance that cannot be reached is not a reason to stay
/// off the LAN: the machine hosts with no relay and no lookup, the device
/// fetches the same manifest and fails the same way, and pairing and
/// connecting over loopback still work. The official service is never
/// substituted on either side.
#[test]
fn an_unreachable_self_hosted_instance_still_lets_the_lan_pair_and_connect() {
    let host_dir = TestDir::new("dead-traverse-host");
    let (mux, _, _) = fake_host();
    let dead = url::Url::from_file_path(host_dir.0.join("missing").join("relays.json")).unwrap();
    let host = TraverseHost::start(
        mux,
        HostConfig {
            host_name: "Test Host".into(),
            data_dir: host_dir.0.clone(),
            traverse: TraverseMode::Custom(dead.clone()),
            pairing_enabled: true,
            bind_port: None,
        },
    )
    .expect("hosting starts without the manifest");
    assert_eq!(host.endpoint_id().len(), 64);
    assert!(host.addr().relays.is_empty(), "no relay in hand");
    let dir = TestDir::new("dead-traverse-phone");
    let phone = device(&dir, "phone");
    let minted = host.new_invitation();
    assert_eq!(minted.invite.traverse.as_deref(), Some(dead.as_str()));
    let paired = tcode_traverse::pair_blocking(&minted.invite, &phone).unwrap();
    assert_eq!(paired.traverse.as_deref(), Some(dead.as_str()));
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, ConnectionState::Syncing);
    client.to_host.send_blocking(subscribe(1)).unwrap();
    recv_type(&client, "ack", Some(1));
    wait_state(&client, connected_directly());
    assert!(
        phone.relays().is_empty(),
        "the device took no relay from anywhere"
    );
    client.to_host.close();
    host.shutdown();
}

/// The heartbeat is the transport's own line, never charged to the outbox.
/// Crediting it on send underflowed the outbox count after `NATIVE_IDLE_MS`
/// of silence and killed the writer, so a connection that went quiet for ten
/// seconds silently stopped carrying commands while still reporting
/// `Connected`.
#[test]
fn an_idle_connection_survives_its_own_heartbeat_and_still_carries_commands() {
    let host_dir = TestDir::new("idle-host");
    let (mux, _, _) = fake_host();
    let host = start_host(mux, &host_dir, None);
    let dir = TestDir::new("idle-device");
    let phone = device(&dir, "phone");
    let minted = host.new_invitation();
    let paired = tcode_traverse::pair_blocking(&minted.invite, &phone).unwrap();
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, ConnectionState::Syncing);
    client.to_host.send_blocking(subscribe(1)).unwrap();
    recv_type(&client, "ack", Some(1));
    wait_state(&client, connected_directly());

    std::thread::sleep(Duration::from_millis(NATIVE_IDLE_MS + 2_000));
    while let Ok(state) = client.state.try_recv() {
        assert_eq!(state, connected_directly(), "the idle link stayed up");
    }
    let ping = json!({"id": 2, "payload": {"type": "query", "content": {"type": "ping"}}});
    client.to_host.send_blocking(ping.to_string()).unwrap();
    recv_type(&client, "query_result", Some(2));
    assert_eq!(client.state.try_recv().ok(), None);
    host.shutdown();
}

#[test]
fn two_devices_route_acks_broadcast_events_and_scope_keys() {
    let host_dir = TestDir::new("route-host");
    let (mux, _, keys) = fake_host();
    let host = start_host(mux, &host_dir, None);
    let dir_a = TestDir::new("route-a");
    let dir_b = TestDir::new("route-b");
    let device_a = device(&dir_a, "A");
    let device_b = device(&dir_b, "B");
    let minted = host.new_invitation();
    let host_a = tcode_traverse::pair_blocking(&minted.invite, &device_a).unwrap();
    let minted = host.new_invitation();
    let host_b = tcode_traverse::pair_blocking(&minted.invite, &device_b).unwrap();
    let client_a = tcode_traverse::connect(&host_a, &device_a);
    let client_b = tcode_traverse::connect(&host_b, &device_b);
    wait_state(&client_a, ConnectionState::Syncing);
    wait_state(&client_b, ConnectionState::Syncing);
    client_a.to_host.send_blocking(subscribe(10)).unwrap();
    client_b.to_host.send_blocking(subscribe(20)).unwrap();
    recv_type(&client_a, "event", None);
    recv_type(&client_b, "event", None);
    recv_type(&client_a, "ack", Some(10));
    recv_type(&client_b, "ack", Some(20));
    wait_state(&client_a, connected_directly());
    wait_state(&client_b, connected_directly());
    let live = host.devices();
    assert!(
        live.iter()
            .all(|device| device.live.as_ref().is_some_and(|live| live.direct)),
        "loopback connections are direct: {live:?}"
    );

    let create = json!({
        "id": 11,
        "key": "3f2b8c6e-1d4a-4b9e-8c7d-2a1f0e9d8c7b",
        "payload": {"type": "command", "content": {"type": "create_project", "content": {"root": "/tmp/project"}}}
    })
    .to_string();
    client_a.to_host.send_blocking(create).unwrap();
    recv_type(&client_a, "event", None);
    recv_type(&client_b, "event", None);
    recv_type(&client_a, "ack", Some(11));
    assert_eq!(
        keys.lock().unwrap().as_slice(),
        [format!(
            "{}:3f2b8c6e-1d4a-4b9e-8c7d-2a1f0e9d8c7b",
            device_a.endpoint_id()
        )],
        "keys are scoped by the authenticated device"
    );
    // A reply to B, ordered after A's command by the host, bounds the absence check.
    client_b
        .to_host
        .send_blocking(
            json!({"id": 21, "payload": {"type":"command", "content":{"type":"open_latest_session"}}})
                .to_string(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "B's command barrier was not acknowledged"
        );
        let Ok(line) = client_b.from_host.try_recv() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_ne!(
            value["content"]["id"].as_u64(),
            Some(11),
            "A's acknowledgment leaked to B"
        );
        if value["type"] == "ack" && value["content"]["id"] == 21 {
            break;
        }
    }
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
}

/// A revocation is durable or it did not happen: while the allow list
/// cannot be written, the device stays listed, its connection stays open and
/// it is still admitted, and the caller is told. Once it is written, the
/// live connection closes and a reconnect is refused.
#[test]
fn revocation_closes_the_live_connection_and_rejects_reconnects() {
    let host_dir = TestDir::new("revoke-host");
    let (mux, _, _) = fake_host();
    let host = start_host(mux, &host_dir, None);
    let dir = TestDir::new("revoke-phone");
    let phone = device(&dir, "phone");
    let minted = host.new_invitation();
    let paired = tcode_traverse::pair_blocking(&minted.invite, &phone).unwrap();
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, ConnectionState::Syncing);
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
    wait_state(&again, ConnectionState::Syncing);
    again.to_host.close();
    std::fs::remove_dir(&blocker).unwrap();

    host.revoke(&phone.endpoint_id().to_string()).unwrap();
    wait_state(
        &client,
        ConnectionState::Offline {
            reason: ConnectionFailure::AuthenticationRejected,
        },
    );
    assert!(host.devices().is_empty());
    client.to_host.close();
    let again = tcode_traverse::connect(&paired, &phone);
    wait_state(
        &again,
        ConnectionState::Offline {
            reason: ConnectionFailure::AuthenticationRejected,
        },
    );
    again.to_host.close();
    host.shutdown();
}

#[test]
fn a_restarted_machine_is_rejoined_and_buffered_writes_are_delivered() {
    let host_dir = TestDir::new("restart-host");
    let (mux, subscribe_count, _) = fake_host();
    let port = {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().port()
    };
    let host = start_host(mux.clone(), &host_dir, Some(port));
    let dir = TestDir::new("restart-phone");
    let phone = device(&dir, "phone");
    let minted = host.new_invitation();
    let paired = tcode_traverse::pair_blocking(&minted.invite, &phone).unwrap();
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, ConnectionState::Syncing);
    client.to_host.send_blocking(subscribe(1)).unwrap();
    recv_type(&client, "event", None);
    wait_state(&client, connected_directly());
    let endpoint_id = host.endpoint_id();

    host.shutdown();
    wait_state(
        &client,
        ConnectionState::Reconnecting {
            // This link lived less than 30 seconds, so it must not reset retry history.
            attempt: 2,
            reason: Some(ConnectionFailure::HostClosed),
        },
    );
    client
        .to_host
        .send_blocking(
            json!({"id": 2, "payload": {"type":"command", "content":{"type":"open_latest_session"}}})
                .to_string(),
        )
        .unwrap();
    let restarted = start_host(mux, &host_dir, Some(port));
    assert_eq!(
        restarted.endpoint_id(),
        endpoint_id,
        "same key, same identity"
    );
    wait_state(&client, connected_directly());
    let deadline = Instant::now() + Duration::from_secs(5);
    while subscribe_count.load(Ordering::Relaxed) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(subscribe_count.load(Ordering::Relaxed), 2);
    recv_type(&client, "ack", Some(2));
    client.to_host.close();
    restarted.shutdown();
}
