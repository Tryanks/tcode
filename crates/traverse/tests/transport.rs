//! Two in-process endpoints over loopback: no relay, no discovery, no network.
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tcode_client::host::Transport;
use tcode_client::{ConnectionFailure, ConnectionState};
use tcode_traverse::{
    DeviceIdentity, EndpointOptions, HostConfig, HostMux, PairError, TraverseHost, TraverseMode,
};

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
            lan_discovery: false,
            bind_port,
        },
    )
    .unwrap()
}

fn device(dir: &TestDir, name: &str) -> DeviceIdentity {
    let device = DeviceIdentity::load_or_create(&dir.0)
        .unwrap()
        .with_options(EndpointOptions {
            official: false,
            lan_discovery: false,
        });
    device.set_details(name.into(), None);
    device
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
fn pairing_is_single_use_five_failures_invalidate_and_unpaired_devices_are_rejected() {
    let host_dir = TestDir::new("pair-host");
    let (mux, _, _) = fake_host();
    let host = start_host(mux, &host_dir, None);
    let phone_dir = TestDir::new("pair-phone");
    let phone = device(&phone_dir, "phone");
    let other_dir = TestDir::new("pair-other");
    let other = device(&other_dir, "other");

    let minted = host.new_pairing_code();
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
    for _ in 0..5 {
        assert_eq!(
            tcode_traverse::pair_blocking(invite, "000000", &phone),
            Err(PairError::Code)
        );
    }
    assert_eq!(
        tcode_traverse::pair_blocking(invite, &minted.code, &phone),
        Err(PairError::Code),
        "five failures burn the code even for the right digits"
    );
    assert!(host.devices().is_empty());

    let minted = host.new_pairing_code();
    let paired = tcode_traverse::pair_blocking(&minted.invite, &minted.code, &phone).unwrap();
    assert_eq!(paired.host_id, host.endpoint_id());
    assert_eq!(paired.name, "Test Host");
    assert_eq!(paired.addrs, minted.invite.addrs);
    assert_eq!(
        tcode_traverse::pair_blocking(&minted.invite, &minted.code, &other),
        Err(PairError::Code),
        "a code is single use"
    );
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
    let minted = host.new_pairing_code();
    assert_eq!(
        tcode_traverse::pair_blocking(&minted.invite, &minted.code, &other),
        Err(PairError::Disabled)
    );
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
    let minted = host.new_pairing_code();
    let host_a = tcode_traverse::pair_blocking(&minted.invite, &minted.code, &device_a).unwrap();
    let minted = host.new_pairing_code();
    let host_b = tcode_traverse::pair_blocking(&minted.invite, &minted.code, &device_b).unwrap();
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
    wait_state(&client_a, ConnectionState::Connected);
    wait_state(&client_b, ConnectionState::Connected);
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

#[test]
fn revocation_closes_the_live_connection_and_rejects_reconnects() {
    let host_dir = TestDir::new("revoke-host");
    let (mux, _, _) = fake_host();
    let host = start_host(mux, &host_dir, None);
    let dir = TestDir::new("revoke-phone");
    let phone = device(&dir, "phone");
    let minted = host.new_pairing_code();
    let paired = tcode_traverse::pair_blocking(&minted.invite, &minted.code, &phone).unwrap();
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, ConnectionState::Syncing);
    client.to_host.send_blocking(subscribe(1)).unwrap();
    recv_type(&client, "ack", Some(1));
    assert!(host.devices()[0].live.is_some());

    host.revoke(&phone.endpoint_id().to_string());
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
    let minted = host.new_pairing_code();
    let paired = tcode_traverse::pair_blocking(&minted.invite, &minted.code, &phone).unwrap();
    let client = tcode_traverse::connect(&paired, &phone);
    wait_state(&client, ConnectionState::Syncing);
    client.to_host.send_blocking(subscribe(1)).unwrap();
    recv_type(&client, "event", None);
    wait_state(&client, ConnectionState::Connected);
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
    wait_state(&client, ConnectionState::Connected);
    let deadline = Instant::now() + Duration::from_secs(5);
    while subscribe_count.load(Ordering::Relaxed) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(subscribe_count.load(Ordering::Relaxed), 2);
    recv_type(&client, "ack", Some(2));
    client.to_host.close();
    restarted.shutdown();
}
