use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tcode_remote::client::{ConnectionState, PairedHost, connect, pair};
use tcode_remote::{HostMux, RemoteConfig, serve};
use tungstenite::Message;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tcode-remote-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fake_host() -> (HostMux, Arc<AtomicUsize>) {
    let (to_host, host_rx) = async_channel::unbounded::<String>();
    let (host_tx, from_host) = async_channel::unbounded::<String>();
    let subscribe_count = Arc::new(AtomicUsize::new(0));
    let count = subscribe_count.clone();
    std::thread::spawn(move || {
        while let Ok(line) = host_rx.recv_blocking() {
            let value: Value = serde_json::from_str(line.trim_end()).unwrap();
            let id = value["id"].as_u64().unwrap();
            let kind = value["payload"]["type"].as_str().unwrap();
            if kind == "subscribe" {
                count.fetch_add(1, Ordering::Relaxed);
                host_tx
                    .send_blocking(
                        json!({
                            "type": "event",
                            "content": {
                                "topic": "index",
                                "event": {"type": "index_snapshot", "content": {"sessions": [], "projects": []}}
                            }
                        })
                        .to_string(),
                    )
                    .unwrap();
            }
            if kind == "command"
                && value["payload"]["content"]["type"].as_str() == Some("create_project")
            {
                host_tx
                    .send_blocking(
                        json!({
                            "type": "event",
                            "content": {
                                "topic": "index",
                                "event": {"type": "index_snapshot", "content": {"sessions": [], "projects": []}}
                            }
                        })
                        .to_string(),
                    )
                    .unwrap();
            }
            host_tx
                .send_blocking(
                    json!({
                        "type": "ack",
                        "content": {"id": id, "result": {"Ok": {"type": "unit"}}}
                    })
                    .to_string(),
                )
                .unwrap();
        }
    });
    (HostMux::new(to_host, from_host), subscribe_count)
}

fn config(data_dir: PathBuf, port: u16) -> RemoteConfig {
    RemoteConfig {
        listen: format!("127.0.0.1:{port}").parse().unwrap(),
        host_name: "Test Host".into(),
        data_dir,
        static_bundle: None,
    }
}

fn wait_state(client: &tcode_remote::client::RemoteClient, wanted: ConnectionState) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(state) = client.state.try_recv()
            && state == wanted
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("did not observe state {wanted:?}");
}

fn recv_type(client: &tcode_remote::client::RemoteClient, kind: &str, id: Option<u64>) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(line) = client.from_host.try_recv() {
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

#[test]
fn pairing_is_single_use_and_five_failures_invalidate() {
    let data = TestDir::new();
    let (mux, _) = fake_host();
    let server = serve(mux, config(data.0.clone(), 0)).unwrap();
    let port = server.local_addr().port();
    let code = server.new_pairing_code();
    let paired = pair("127.0.0.1", port, &code.code, "phone").unwrap();
    assert!(!paired.token.is_empty());
    assert!(pair("127.0.0.1", port, &code.code, "again").is_err());

    let code = server.new_pairing_code();
    let wrong = if code.code == "999999" {
        "000000"
    } else {
        "999999"
    };
    for _ in 0..5 {
        assert!(pair("127.0.0.1", port, wrong, "attacker").is_err());
    }
    assert!(pair("127.0.0.1", port, &code.code, "phone").is_err());
    server.shutdown();
}

#[test]
fn two_clients_route_acks_broadcast_events_and_reconnect() {
    let data = TestDir::new();
    let (mux, subscribe_count) = fake_host();
    let server = serve(mux.clone(), config(data.0.clone(), 0)).unwrap();
    let port = server.local_addr().port();
    let code_a = server.new_pairing_code();
    let host_a = pair("127.0.0.1", port, &code_a.code, "A").unwrap();
    let code_b = server.new_pairing_code();
    let host_b = pair("127.0.0.1", port, &code_b.code, "B").unwrap();
    let client_a = connect(host_a, "A".into());
    let client_b = connect(host_b, "B".into());
    wait_state(&client_a, ConnectionState::Connected);
    wait_state(&client_b, ConnectionState::Connected);
    let subscribe = |id| {
        json!({"id": id, "payload": {"type": "subscribe", "content": {"topic": "index"}}})
            .to_string()
    };
    client_a.to_host.send_blocking(subscribe(10)).unwrap();
    client_b.to_host.send_blocking(subscribe(20)).unwrap();
    recv_type(&client_a, "event", None);
    recv_type(&client_b, "event", None);
    recv_type(&client_a, "ack", Some(10));
    recv_type(&client_b, "ack", Some(20));
    let create = json!({
        "id": 11,
        "payload": {"type": "command", "content": {"type": "create_project", "content": {"root": "/tmp/project"}}}
    })
    .to_string();
    client_a.to_host.send_blocking(create).unwrap();
    recv_type(&client_a, "event", None);
    recv_type(&client_b, "event", None);
    recv_type(&client_a, "ack", Some(11));
    let no_ack_deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < no_ack_deadline {
        if let Ok(line) = client_b.from_host.try_recv() {
            let value: Value = serde_json::from_str(line.trim_end()).unwrap();
            assert_ne!(value["content"]["id"].as_u64(), Some(11));
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    server.shutdown();
    wait_state(&client_a, ConnectionState::Reconnecting { attempt: 1 });
    let restarted = serve(mux, config(data.0.clone(), port)).unwrap();
    wait_state(&client_a, ConnectionState::Connected);
    let deadline = Instant::now() + Duration::from_secs(5);
    while subscribe_count.load(Ordering::Relaxed) < 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(subscribe_count.load(Ordering::Relaxed) >= 3);
    recv_type(&client_a, "event", None);
    restarted.shutdown();
}

#[test]
fn wrong_token_gets_rejected_and_closed() {
    let data = TestDir::new();
    let (mux, _) = fake_host();
    let server = serve(mux, config(data.0.clone(), 0)).unwrap();
    let port = server.local_addr().port();
    smol::block_on(async {
        let stream = smol::Async::<std::net::TcpStream>::connect(([127, 0, 0, 1], port))
            .await
            .unwrap();
        let (mut websocket, _) =
            async_tungstenite::client_async(format!("ws://127.0.0.1:{port}/ws"), stream)
                .await
                .unwrap();
        websocket
            .send(Message::Text(
                json!({"type": "hello", "protocol_version": 1, "token": "wrong"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let reply = websocket.next().await.unwrap().unwrap();
        let Message::Text(reply) = reply else {
            panic!("expected text rejection");
        };
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["type"], "hello_rejected");
        assert!(matches!(
            websocket.next().await,
            None | Some(Ok(Message::Close(_)))
        ));
    });
    server.shutdown();
}

#[test]
fn devices_are_listed_and_revoking_refuses_the_token() {
    let data = TestDir::new();
    let (mux, _) = fake_host();
    let server = serve(mux, config(data.0.clone(), 0)).unwrap();
    let port = server.local_addr().port();
    let code = server.new_pairing_code();
    let paired = pair("127.0.0.1", port, &code.code, "laptop").unwrap();

    let devices = server.devices();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "laptop");
    assert!(devices[0].created_unix > 0);

    assert!(server.revoke_device(&devices[0].id).unwrap());
    assert!(server.devices().is_empty());
    // A second revoke of the same id is a no-op, not an error.
    assert!(!server.revoke_device(&devices[0].id).unwrap());

    smol::block_on(async {
        let stream = smol::Async::<std::net::TcpStream>::connect(([127, 0, 0, 1], port))
            .await
            .unwrap();
        let (mut websocket, _) =
            async_tungstenite::client_async(format!("ws://127.0.0.1:{port}/ws"), stream)
                .await
                .unwrap();
        websocket
            .send(Message::Text(
                json!({"type": "hello", "protocol_version": 1, "token": paired.token})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let Some(Ok(Message::Text(reply))) = websocket.next().await else {
            panic!("expected text rejection");
        };
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["type"], "hello_rejected");
    });
    server.shutdown();
}

#[test]
fn paired_host_shape_is_public() {
    let host = PairedHost {
        host_id: "id".into(),
        name: "name".into(),
        addrs: vec!["127.0.0.1".into()],
        port: 1,
        token: "token".into(),
        last_connected_unix: None,
    };
    assert_eq!(host.port, 1);
}
