use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tcode_remote::client::{ConnectionFailure, ConnectionState, connect, pair};
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
    let paired = pair(&format!("http://127.0.0.1:{}", port), &code.code, "phone").unwrap();
    assert!(!paired.token.is_empty());
    assert!(pair(&format!("http://127.0.0.1:{}", port), &code.code, "again").is_err());

    let code = server.new_pairing_code();
    let wrong = if code.code == "999999" {
        "000000"
    } else {
        "999999"
    };
    for _ in 0..5 {
        assert!(pair(&format!("http://127.0.0.1:{}", port), wrong, "attacker").is_err());
    }
    assert!(pair(&format!("http://127.0.0.1:{}", port), &code.code, "phone").is_err());
    server.shutdown();
}

#[test]
fn two_clients_route_acks_broadcast_events_and_reconnect() {
    let data = TestDir::new();
    let (mux, subscribe_count) = fake_host();
    let server = serve(mux.clone(), config(data.0.clone(), 0)).unwrap();
    let port = server.local_addr().port();
    let code_a = server.new_pairing_code();
    let host_a = pair(&format!("http://127.0.0.1:{}", port), &code_a.code, "A").unwrap();
    let code_b = server.new_pairing_code();
    let host_b = pair(&format!("http://127.0.0.1:{}", port), &code_b.code, "B").unwrap();
    let client_a = connect(host_a, "A".into());
    let client_b = connect(host_b, "B".into());
    wait_state(&client_a, ConnectionState::Syncing);
    wait_state(&client_b, ConnectionState::Syncing);
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
    wait_state(&client_a, ConnectionState::Connected);
    wait_state(&client_b, ConnectionState::Connected);
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
    wait_state(
        &client_a,
        ConnectionState::Reconnecting {
            // This socket lived less than 30 seconds, so it must not reset retry history.
            attempt: 2,
            reason: Some(ConnectionFailure::HostClosed),
        },
    );
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
                json!({"type": "hello", "protocol_version": tcode_protocol::PROTOCOL_VERSION, "token": "wrong"})
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
        assert_eq!(reply["reason"], "token");
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
    let paired = pair(&format!("http://127.0.0.1:{}", port), &code.code, "laptop").unwrap();

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
                json!({"type": "hello", "protocol_version": tcode_protocol::PROTOCOL_VERSION, "token": paired.token})
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
        assert_eq!(reply["reason"], "token");
    });
    server.shutdown();
}

#[test]
fn static_bundle_get_and_head_share_headers() {
    use std::io::{Read as _, Write as _};
    let dir = TestDir::new();
    let (mux, _) = fake_host();
    let mut config = config(dir.0.clone(), 0);
    config.static_bundle = Some(&[
        ("/index.html", b"<html>tcode</html>"),
        ("/tcode_web_bg.wasm", b"\0asm"),
    ]);
    let server = serve(mux, config).unwrap();
    for (path, status, content_type, length) in [
        ("/", "200 OK", "text/html; charset=utf-8", 18),
        ("/index.html", "200 OK", "text/html; charset=utf-8", 18),
        ("/tcode_web_bg.wasm", "200 OK", "application/wasm", 4),
        ("/missing", "404 Not Found", "text/plain; charset=utf-8", 9),
    ] {
        let request = |method| {
            let stream = std::net::TcpStream::connect(server.local_addr()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = stream;
            write!(
                stream,
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            )
            .unwrap();
            let mut result = Vec::new();
            let read = stream.read_to_end(&mut result);
            assert!(read.is_ok() || read.unwrap_err().kind() == std::io::ErrorKind::UnexpectedEof);
            String::from_utf8(result).unwrap()
        };
        let get = request("GET");
        let head = request("HEAD");
        let (headers, body) = get.split_once("\r\n\r\n").unwrap();
        assert_eq!(head, format!("{headers}\r\n\r\n"));
        assert!(headers.starts_with(&format!("HTTP/1.1 {status}")));
        assert!(headers.contains(&format!("Content-Type: {content_type}")));
        assert!(headers.contains(&format!("Content-Length: {length}")));
        assert_eq!(body.len(), length);
    }
    server.shutdown();
}

#[test]
fn admin_pair_is_plain_http_and_creates_no_certificate_identity() {
    let data = TestDir::new();
    let (mux, _) = fake_host();
    let server = serve(mux, config(data.0.clone(), 0)).unwrap();
    let origin = format!("http://{}", server.local_addr());
    let bytes = tcode_remote::client::http(&origin, "GET", "/admin/pair", "").unwrap();
    let reply: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(reply.get("fp").is_none());
    assert_eq!(
        reply["browser_url"],
        format!("{origin}/#code={}", reply["code"].as_str().unwrap())
    );
    assert!(!data.0.join("remote-cert.der").exists());
    assert!(!data.0.join("remote-key.der").exists());
    let host = pair(&origin, reply["code"].as_str().unwrap(), "phone").unwrap();
    let client_data = TestDir::new();
    tcode_remote::client::save_hosts(&client_data.0, std::slice::from_ref(&host)).unwrap();
    assert_eq!(
        tcode_remote::client::load_hosts(&client_data.0).unwrap(),
        [host]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for path in [data.0.join("remote.json"), client_data.0.join("hosts.json")] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    server.shutdown();
}

#[test]
fn upgrade_stall_uses_the_remaining_handshake_budget() {
    let data = TestDir::new();
    let (mux, _) = fake_host();
    let server = serve(mux, config(data.0.clone(), 0)).unwrap();
    let code = server.new_pairing_code();
    let mut host = pair(
        &format!("http://127.0.0.1:{}", server.local_addr().port()),
        &code.code,
        "deadline",
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    host.origin = format!("http://{}", listener.local_addr().unwrap());
    let (upgraded, saw_tcp) = async_channel::bounded(1);
    let (release, held) = async_channel::bounded::<()>(1);
    let fixture = std::thread::spawn(move || {
        smol::block_on(async {
            let listener = smol::Async::new(listener).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let _stream = stream;
            upgraded.send(()).await.unwrap();
            // Keep the TCP connection open, but never answer the upgrade.
            let _ = held.recv().await;
        })
    });
    let start = Instant::now();
    let client = connect(host, "deadline".into());
    saw_tcp.recv_blocking().unwrap();
    smol::block_on(async {
        futures_lite::future::race(
            async {
                loop {
                    if let ConnectionState::Reconnecting {
                        reason: Some(reason),
                        ..
                    } = client.state.recv().await.unwrap()
                    {
                        assert_eq!(reason, ConnectionFailure::Timeout);
                        break;
                    }
                }
            },
            async {
                smol::Timer::after(Duration::from_millis(15_250)).await;
                panic!("upgrade escaped the handshake deadline");
            },
        )
        .await;
    });
    assert!(start.elapsed() < Duration::from_millis(15_250));
    client.to_host.close();
    release.close();
    fixture.join().unwrap();
    server.shutdown();
}
