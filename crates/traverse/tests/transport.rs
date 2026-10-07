//! Two in-process endpoints using native direct-path selection, without a relay.
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
        while let Ok(line) = host_rx.recv_blocking() {
            let request = tcode_protocol::decode_client_line(&line).unwrap();
            let id = request.id;
            if let Some(key) = request.key {
                seen_keys.lock().unwrap().push(key);
            }
            if let tcode_protocol::ClientPayload::Subscribe(subscription) = &request.payload {
                count.fetch_add(1, Ordering::Relaxed);
                let snapshot = tcode_protocol::HostMessage::Event(tcode_protocol::EventEnvelope {
                    request_id: Some(id),
                    topic: subscription.topic.clone(),
                    event: tcode_protocol::ServerEvent::IndexSnapshot(
                        tcode_protocol::IndexSnapshot {
                            summary: tcode_protocol::IndexSummary::default(),
                            sessions: Vec::new(),
                            projects: Vec::new(),
                        },
                    ),
                });
                host_tx
                    .send_blocking(tcode_protocol::encode_line(&snapshot).unwrap())
                    .unwrap();
            }
            if let tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Ping) =
                &request.payload
            {
                host_tx
                    .send_blocking(
                        tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                            id,
                            result: Ok(tcode_protocol::QueryResponse::Pong),
                        })
                        .unwrap(),
                    )
                    .unwrap();
                continue;
            }
            let mut response = tcode_protocol::CommandResponse::Unit;
            if let tcode_protocol::ClientPayload::Command(
                tcode_protocol::Command::CreateProject { root },
            ) = &request.payload
            {
                let broadcast = tcode_protocol::HostMessage::Event(tcode_protocol::EventEnvelope {
                    request_id: None, topic: tcode_protocol::Topic::Index,
                    event: serde_json::from_value(json!({"type":"index_upsert_project","content":{"id":"project","name":"Shared","root":root,"created_at":1}})).unwrap(),
                });
                host_tx
                    .send_blocking(tcode_protocol::encode_line(&broadcast).unwrap())
                    .unwrap();
                response = tcode_protocol::CommandResponse::ProjectId(Some("project".into()));
            }
            host_tx
                .send_blocking(
                    tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                        id,
                        result: Ok(response),
                    })
                    .unwrap(),
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

fn wait_state(transport: &Transport, wanted: impl Fn(&ConnectionState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Ok(state) = transport.state.try_recv() {
            if wanted(&state) {
                return;
            }
            seen.push(state);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("did not observe the wanted state; saw {seen:?}");
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
    tcode_protocol::encode_line(&tcode_protocol::ClientMessage {
        id,
        key: None,
        principal: None,
        payload: tcode_protocol::ClientPayload::Subscribe(tcode_protocol::Subscription {
            topic: tcode_protocol::Topic::Index,
            after: None,
        }),
    })
    .unwrap()
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
    wait_state(&stranger, |state| {
        state
            == &ConnectionState::Offline {
                reason: ConnectionFailure::AuthenticationRejected,
            }
    });
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
/// service brings the official relays, a custom instance adds its own, and
/// removing those machines takes their relays away again. Fresh manifest
/// caches keep discovery local to the test.
#[test]
fn device_relays_follow_the_traverse_instances_of_its_machines() {
    let off_dir = TestDir::new("relays-off-host");
    let (mux, _, _) = fake_host();
    let off_host = start_host(mux, &off_dir, None);
    let official_dir = TestDir::new("relays-official-host");
    let (mux, _, _) = fake_host();
    let official_host = start_host(mux, &official_dir, None);
    let custom_dir = TestDir::new("relays-custom-host");
    let (mux, _, _) = fake_host();
    let custom_host = start_host(mux, &custom_dir, None);
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
    let custom_base: url::Url = "https://custom.traverse.test/".parse().unwrap();
    std::fs::write(
        tcode_traverse::manifest::ManifestSource::Custom(custom_base.clone()).cache_path(&dir.0),
        json!({
            "fetchedAtMs": now_ms,
            "manifest": {
                "version": 1,
                "updatedAt": "2999-01-01T00:00:00Z",
                "relays": [{"url": "https://custom.relay.test/"}],
                "pkarr": ["https://custom.lookup.test/pkarr"]
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
    // Pair this machine while Off, so its custom source can only be loaded
    // by the saved-host reconciliation below, not by pairing itself.
    let mut custom =
        tcode_traverse::pair_blocking(&custom_host.new_invitation().invite, &phone).unwrap();
    custom.traverse = Some(custom_base.to_string());
    tcode_traverse::hosts::save_hosts(&dir.0, &[off.clone(), official, custom]).unwrap();
    phone.hosts_changed();
    // The new relay proves reconciliation ran; the saved official source
    // must survive that same update.
    wait_relays(
        &phone,
        &["https://custom.relay.test/", "https://official.relay.test/"],
    );

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
    wait_state(&client, syncing_directly);
    client.to_host.send_blocking(subscribe(1)).unwrap();
    recv_type(&client, "ack", Some(1));
    wait_state(&client, connected_directly);

    std::thread::sleep(Duration::from_millis(NATIVE_IDLE_MS + 2_000));
    while let Ok(state) = client.state.try_recv() {
        assert!(
            connected_directly(&state),
            "the idle link stayed up: {state:?}"
        );
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
    phone.set_details("phone".into(), Some("Android 15".into()));
    let minted = host.new_invitation();
    let paired = tcode_traverse::pair_blocking(&minted.invite, &phone).unwrap();
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, syncing_directly);
    client.to_host.send_blocking(subscribe(1)).unwrap();
    recv_type(&client, "event", None);
    wait_state(&client, connected_directly);
    let endpoint_id = host.endpoint_id();

    host.set_pairing_enabled(false);
    host.shutdown();
    wait_state(&client, |state| {
        state
            == &ConnectionState::Reconnecting {
                // This link lived less than 30 seconds, so it must not reset retry history.
                attempt: 2,
                reason: Some(ConnectionFailure::HostClosed),
            }
    });
    client
        .to_host
        .send_blocking(
            json!({"id": 2, "payload": {"type":"command", "content":{"type":"open_latest_session"}}})
                .to_string(),
        )
        .unwrap();
    let restarted = TraverseHost::start(
        mux,
        HostConfig {
            host_name: "Renamed".into(),
            data_dir: host_dir.0.clone(),
            traverse: TraverseMode::Off,
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
    wait_state(&client, connected_directly);
    let deadline = Instant::now() + Duration::from_secs(5);
    while subscribe_count.load(Ordering::Relaxed) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(subscribe_count.load(Ordering::Relaxed), 2);
    recv_type(&client, "ack", Some(2));
    client.to_host.close();
    restarted.shutdown();
}

fn space_invite(host: &TraverseHost, id: &str) -> PairInvite {
    tcode_client::pairing::parse_pair_url(&host.space_link_url(id).unwrap()).unwrap()
}

fn scoped_ping(
    client: &Transport,
    host_rx: &async_channel::Receiver<String>,
    host_tx: &async_channel::Sender<String>,
    principal: &tcode_protocol::Principal,
    id: u64,
) {
    client
        .to_host
        .send_blocking(
            tcode_protocol::encode_line(&tcode_protocol::ClientMessage {
                id,
                key: None,
                principal: Some(tcode_protocol::Principal::Full),
                payload: tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Ping),
            })
            .unwrap(),
        )
        .unwrap();
    let line = tcode_traverse::block_on(async {
        tokio::time::timeout(Duration::from_secs(10), host_rx.recv())
            .await
            .unwrap()
            .unwrap()
    });
    let request = tcode_protocol::decode_client_line(&line).unwrap();
    assert_eq!(request.principal.as_ref(), Some(principal));
    assert_eq!(
        request.payload,
        tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Ping)
    );
    host_tx
        .send_blocking(
            tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                id: request.id,
                result: Ok(tcode_protocol::QueryResponse::Pong),
            })
            .unwrap(),
        )
        .unwrap();
    let reply = recv_type(client, "query_result", Some(id));
    assert_eq!(reply["content"]["result"]["Ok"]["type"], "pong");
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
    let id = host.create_space("Shared".into()).unwrap();
    host.set_space_projects(&id, vec!["project-a".into(), "project-b".into()])
        .unwrap();
    let invite = space_invite(&host, &id);
    assert_eq!(invite.space.as_deref(), Some(id.as_str()));
    let member_dir = TestDir::new("spaces-member");
    let member = device(&member_dir, "Collaborator");
    let other_dir = TestDir::new("spaces-other");
    let other = device(&other_dir, "Other");
    let paired = tcode_traverse::pair_blocking(&invite, &member).unwrap();
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
        space_id: id.clone(),
        space_name: "Shared".into(),
        project_ids: vec!["project-a".into(), "project-b".into()],
        device_id: member.endpoint_id().to_string(),
        device_name: "Collaborator".into(),
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
    assert_eq!(
        tcode_traverse::pair_blocking(&invite, &member),
        Err(PairError::AlreadyMember)
    );

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
    let opener = client.current_host.as_ref().unwrap().tunnels().unwrap();
    let refusal = tcode_traverse::block_on(opener.open("127.0.0.1", 1)).unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("space members cannot open tunnels"),
        "{refusal}"
    );

    host.set_space_link_enabled(&id, false).unwrap();
    assert!(host.space_link_url(&id).is_none());
    assert_eq!(
        tcode_traverse::pair_blocking(&invite, &other),
        Err(PairError::SpaceUnavailable)
    );
    host.set_space_link_enabled(&id, true).unwrap();
    host.set_pairing_enabled(false);
    assert_eq!(
        tcode_traverse::pair_blocking(&invite, &other),
        Err(PairError::Disabled)
    );
    host.set_pairing_enabled(true);
    let one_shot = host.new_invitation();
    let wrong = PairInvite {
        secret: "AAAAAAAAAAAAAAAAAAAAAA".into(),
        ..invite.clone()
    };
    let blocker = host_dir.0.join("traverse.tmp");
    std::fs::create_dir(&blocker).unwrap();
    assert_eq!(
        tcode_traverse::pair_blocking(&wrong, &other),
        Err(PairError::Busy)
    );
    assert_eq!(
        tcode_traverse::pair_blocking(&invite, &other),
        Err(PairError::Busy)
    );
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
        assert_eq!(
            tcode_traverse::pair_blocking(&wrong, &other),
            Err(PairError::Invalid)
        );
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
    assert_eq!(
        tcode_traverse::pair_blocking(&invite, &other),
        Err(PairError::SpaceUnavailable)
    );
    let full_dir = TestDir::new("spaces-full");
    let full = device(&full_dir, "Owner");
    let full_paired = tcode_traverse::pair_blocking(&one_shot.invite, &full).unwrap();
    assert!(full_paired.space_id.is_none());
    scoped_ping(&client, &host_rx, &host_tx, &principal, 3);
    assert_stayed_connected(&client);

    host.regenerate_space_link(&id).unwrap();
    let regenerated = space_invite(&host, &id);
    assert_eq!(
        tcode_traverse::pair_blocking(&regenerated, &full),
        Err(PairError::AlreadyMember)
    );

    assert_ne!(regenerated.secret, invite.secret);
    assert_eq!(
        tcode_traverse::pair_blocking(&invite, &other),
        Err(PairError::Invalid)
    );
    let other_paired = tcode_traverse::pair_blocking(&regenerated, &other).unwrap();
    let other_client = tcode_traverse::connect(&other_paired, &other);
    wait_state(&other_client, syncing_directly);
    let mut other_principal = principal.clone();
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

    host.remove_member(&member.endpoint_id().to_string())
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
    let repaired = tcode_traverse::pair_blocking(&regenerated, &member).unwrap();
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
    wait_state(&client, |state| {
        matches!(state, ConnectionState::Reconnecting { .. })
    });
    wait_state(&client, syncing_directly);
    if let Principal::Space { project_ids, .. } = &mut principal {
        *project_ids = vec!["project-b".into()];
    }
    scoped_ping(&client, &host_rx, &host_tx, &principal, 7);
    wait_state(&other_client, |state| {
        matches!(state, ConnectionState::Reconnecting { .. })
    });
    wait_state(&other_client, syncing_directly);
    if let Principal::Space { project_ids, .. } = &mut other_principal {
        *project_ids = vec!["project-b".into()];
    }
    scoped_ping(&other_client, &host_rx, &host_tx, &other_principal, 11);
    host.rename_space(&id, "Renamed".into()).unwrap();
    wait_state(&client, |state| {
        matches!(state, ConnectionState::Reconnecting { .. })
    });
    wait_state(&client, syncing_directly);
    if let Principal::Space { space_name, .. } = &mut principal {
        *space_name = "Renamed".into();
    }
    scoped_ping(&client, &host_rx, &host_tx, &principal, 8);
    let moved_space = host.create_space("Moved".into()).unwrap();
    let moved_invite = space_invite(&host, &moved_space);
    assert_eq!(
        tcode_traverse::pair_blocking(&moved_invite, &member),
        Err(PairError::AlreadyMember)
    );
    host.move_member(&member.endpoint_id().to_string(), &moved_space)
        .unwrap();
    wait_state(&client, |state| {
        matches!(state, ConnectionState::Reconnecting { .. })
    });
    wait_state(&client, syncing_directly);
    if let Principal::Space {
        space_id,
        space_name,
        project_ids,
        ..
    } = &mut principal
    {
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
        tcode_traverse::pair_blocking(&regenerated, &member),
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
    let persisted: Value = serde_json::from_slice(&migrated).unwrap();
    assert_eq!(persisted["v"], 3);
    assert_eq!(persisted["devices"][0]["access"], json!({"type":"full"}));
    for access in [
        None,
        Some(json!({"type":"unknown"})),
        Some(json!({"type":"spaces","content":"wrong"})),
        Some(json!({"type":"spaces","content":[]})),
        Some(json!({"type":"spaces","content":[""]})),
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
