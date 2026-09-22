//! The browser listener over loopback: static bundle, password login and the
//! `/ws` hello, as `crates/web` speaks them.
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tcode_traverse::HostMux;
use tcode_traverse::browser::{BrowserConfig, BrowserServer, serve, set_password};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tcode-browser-{tag}-{}-{}",
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

/// Acks every command, answers every subscription with an index event and
/// `create_project` with a broadcast event; records every dedup key.
fn fake_host() -> (HostMux, Arc<std::sync::Mutex<Vec<String>>>) {
    let (to_host, host_rx) = async_channel::unbounded::<String>();
    let (host_tx, from_host) = async_channel::unbounded::<String>();
    let keys = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = keys.clone();
    std::thread::spawn(move || {
        while let Ok(line) = host_rx.recv_blocking() {
            let value: Value = serde_json::from_str(line.trim_end()).unwrap();
            let id = value["id"].as_u64().unwrap();
            if let Some(key) = value["key"].as_str() {
                seen.lock().unwrap().push(key.to_owned());
            }
            if value["payload"]["type"] == "subscribe" {
                host_tx
                    .send_blocking(
                        json!({"type": "event", "content": {"topic": "index", "event": {"type": "index_snapshot", "content": {"sessions": [], "projects": []}}}})
                            .to_string(),
                    )
                    .unwrap();
            }
            if value["payload"]["content"]["type"] == "create_project" {
                host_tx
                    .send_blocking(
                        json!({"type": "event", "content": {"topic": "index", "event": {"type": "project_created", "content": {"path": value["payload"]["content"]["content"]["path"]}}}})
                            .to_string(),
                    )
                    .unwrap();
            }
            host_tx
                .send_blocking(
                    json!({"type": "ack", "content": {"id": id, "result": {"Ok": {"type": "unit"}}}})
                        .to_string(),
                )
                .unwrap();
        }
    });
    (HostMux::new(to_host, from_host), keys)
}

fn config(data_dir: PathBuf) -> BrowserConfig {
    BrowserConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        host_name: "Test Host".into(),
        data_dir,
        static_bundle: None,
        hosting: None,
    }
}

/// One bounded HTTP/1.1 request; returns the status line and body.
fn http(addr: SocketAddr, method: &str, path: &str, body: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes);
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(bytes[..split].to_vec()).unwrap();
    (
        head.lines().next().unwrap().to_owned(),
        bytes[split + 4..].to_vec(),
    )
}

fn login(addr: SocketAddr, password: &str, device_name: &str) -> (String, Value) {
    let body = json!({"password": password, "device_name": device_name, "device_id": format!("{device_name}-id"), "platform": "macOS"});
    let (status, body) = http(addr, "POST", "/auth/login", &body.to_string());
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

type Socket = WebSocketStream<tokio::net::TcpStream>;

/// A browser's WebSocket: the upgrade, then JSON text frames each way.
struct Browser(Socket);

impl Browser {
    async fn open(addr: SocketAddr) -> Self {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (socket, _) = tokio_tungstenite::client_async(format!("ws://{addr}/ws"), stream)
            .await
            .unwrap();
        Self(socket)
    }

    async fn hello(addr: SocketAddr, hello: Value) -> (Self, Value) {
        let mut browser = Self::open(addr).await;
        browser.send(hello).await;
        let reply = browser.recv().await;
        (browser, reply)
    }

    async fn send(&mut self, value: Value) {
        self.0
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> Value {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.0.next())
                .await
                .expect("the listener answered in time")
            {
                Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
                Some(Ok(_)) => continue,
                other => panic!("socket ended: {other:?}"),
            }
        }
    }

    /// Whether the listener closed the socket within `wait`.
    async fn closed_within(&mut self, wait: Duration) -> bool {
        matches!(
            tokio::time::timeout(wait, async {
                loop {
                    match self.0.next().await {
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return true,
                        Some(Ok(_)) => {}
                    }
                }
            })
            .await,
            Ok(true)
        )
    }
}

fn hello(token: &str, device_name: &str) -> Value {
    json!({"type":"hello","protocol_version":tcode_protocol::PROTOCOL_VERSION,"token":token,"device":{"name":device_name,"platform":"macOS"}})
}

fn setup(server: &BrowserServer, password: &str) {
    let (status, _) = http(
        server.local_addr(),
        "POST",
        "/auth/setup",
        &json!({"password": password}).to_string(),
    );
    assert_eq!(status, "HTTP/1.1 200 OK");
}

#[test]
fn static_bundle_get_and_head_share_headers() {
    let dir = TestDir::new("static");
    let mut config = config(dir.0.clone());
    const BUNDLE: &[(&str, &[u8])] = &[
        ("/index.html", b"<html>tcode</html>"),
        ("/tcode_web_bg.wasm", b"\0asm"),
    ];
    config.static_bundle = Some(BUNDLE);
    let server = serve(fake_host().0, config).unwrap();
    for (path, status, content_type, length) in [
        ("/", "200 OK", "text/html; charset=utf-8", 18),
        ("/index.html", "200 OK", "text/html; charset=utf-8", 18),
        ("/tcode_web_bg.wasm", "200 OK", "application/wasm", 4),
        ("/missing", "404 Not Found", "text/plain; charset=utf-8", 9),
    ] {
        let request = |method| {
            let mut stream = TcpStream::connect(server.local_addr()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
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
fn password_setup_login_and_lockout() {
    let dir = TestDir::new("password");
    let server = serve(fake_host().0, config(dir.0.clone())).unwrap();
    let addr = server.local_addr();
    let (status, body) = http(addr, "GET", "/auth/state", "");
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"mode":"password","configured":false})
    );
    assert!(
        login(addr, "secret password", "Browser").0.contains("403"),
        "no login before a password exists"
    );
    assert!(
        http(addr, "POST", "/auth/setup", r#"{"password":"short"}"#)
            .0
            .contains("400")
    );
    let form = TcpStream::connect(addr).unwrap();
    let mut form = form;
    form.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let body = r#"{"password":"secret password"}"#;
    write!(
        form,
        "POST /auth/setup HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    let _ = form.read_to_string(&mut reply);
    assert!(
        reply.starts_with("HTTP/1.1 415 "),
        "a cross-origin form must not claim setup: {reply}"
    );
    setup(&server, "secret password");
    assert!(server.password_configured());
    assert!(
        http(addr, "POST", "/auth/setup", body).0.contains("409"),
        "setup happens once"
    );
    let (status, body) = http(addr, "GET", "/auth/state", "");
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"mode":"password","configured":true})
    );
    let (status, paired) = login(addr, "secret password", "Browser");
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(paired["host_name"], "Test Host");
    assert!(
        paired["token"]
            .as_str()
            .is_some_and(|token| token.len() == 43)
    );
    // The retired native paths are gone from the listener.
    for (method, path) in [
        ("POST", "/pair"),
        ("GET", "/admin/pair"),
        ("POST", "/identity"),
    ] {
        assert_eq!(http(addr, method, path, "{}").0, "HTTP/1.1 404 Not Found");
    }
    for _ in 0..5 {
        assert!(login(addr, "incorrect", "Browser").0.contains("403"));
    }
    assert!(
        login(addr, "secret password", "Browser").0.contains("403"),
        "five failures lock the right password out"
    );
    server.shutdown();
}

#[test]
fn hello_takes_the_current_protocol_only_and_a_revoked_token_closes_the_socket() {
    let dir = TestDir::new("hello");
    let (mux, keys) = fake_host();
    let mut config = config(dir.0.clone());
    let hosting_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = hosting_calls.clone();
    config.hosting = Some(Arc::new(move |action| {
        seen.lock().unwrap().push(format!("{action:?}"));
        tcode_protocol::HostingState {
            enabled: true,
            invite: Some(
                "tcode://pair?v=2&id=machine&secret=AAECAwQFBgcICQoLDA0ODw&name=Test%20Host".into(),
            ),
            expires_in_secs: 299,
            host_id: "machine".into(),
            host_name: "Test Host".into(),
            devices: vec![tcode_protocol::HostedDevice {
                id: "phone".into(),
                name: "Phone".into(),
                created_unix: 1,
                platform: None,
                path: None,
            }],
        }
    }));
    let server = serve(mux, config).unwrap();
    let addr = server.local_addr();
    setup(&server, "secret password");
    let token = login(addr, "secret password", "Chrome").1["token"]
        .as_str()
        .unwrap()
        .to_owned();
    tcode_traverse::block_on(async {
        let (_, rejected) = Browser::hello(
            addr,
            json!({"type":"hello","protocol_version":4,"supported_versions":[3,4],"token":token,"device_name":"Chrome"}),
        )
        .await;
        assert_eq!(rejected["type"], "hello_rejected");
        assert_eq!(rejected["reason"], "protocol");
        let (_, rejected) = Browser::hello(addr, hello("not-a-token", "Chrome")).await;
        assert_eq!(rejected["type"], "hello_rejected");
        assert_eq!(rejected["reason"], "token");

        let (mut browser, hello_ok) = Browser::hello(addr, hello(&token, "Chrome")).await;
        assert_eq!(hello_ok["type"], "hello_ok");
        assert_eq!(
            hello_ok["protocol_version"],
            tcode_protocol::PROTOCOL_VERSION
        );
        assert_eq!(hello_ok["host_name"], "Test Host");
        browser
            .send(json!({"id":900,"payload":{"type":"query","content":{"type":"hosting","content":{"action":{"type":"state"}}}}}))
            .await;
        let reply = browser.recv().await;
        assert_eq!(reply["type"], "query_result");
        assert_eq!(reply["content"]["id"], 900);
        let state = &reply["content"]["result"]["Ok"]["content"];
        assert_eq!(
            state["invite"],
            "tcode://pair?v=2&id=machine&secret=AAECAwQFBgcICQoLDA0ODw&name=Test%20Host",
            "hosting queries are answered by the handler the host installed"
        );
        let devices = state["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 2, "native devices, then this browser");
        assert_eq!(devices[0]["name"], "Phone");
        assert_eq!(devices[1]["name"], "Chrome");
        assert_eq!(devices[1]["platform"], "macOS");
        assert_eq!(
            devices[1]["path"],
            json!({"direct": true, "lan": true}),
            "a connected browser is shown as a device on the machine's own network"
        );
        let browser_id = devices[1]["id"].as_str().unwrap().to_owned();
        assert_eq!(hosting_calls.lock().unwrap().as_slice(), ["State"]);

        // The key is scoped to the browser's token, never to a client-chosen prefix.
        browser
            .send(json!({"id":1,"key":"spoofed:0ec1ee7e-9f37-4bd8-9e04-9a1d84e9a8b4","payload":{"type":"command","content":{"type":"create_project","content":{"path":"/tmp/one"}}}}))
            .await;
        assert!(
            browser.closed_within(Duration::from_secs(5)).await,
            "a malformed key ends the socket"
        );
        let (mut browser, _) = Browser::hello(addr, hello(&token, "Chrome")).await;
        browser
            .send(json!({"id":1,"key":"0ec1ee7e-9f37-4bd8-9e04-9a1d84e9a8b4","payload":{"type":"command","content":{"type":"create_project","content":{"path":"/tmp/one"}}}}))
            .await;
        assert_eq!(browser.recv().await["type"], "ack");
        let keys = keys.lock().unwrap().clone();
        assert_eq!(keys.len(), 1);
        assert!(
            keys[0].ends_with(":0ec1ee7e-9f37-4bd8-9e04-9a1d84e9a8b4")
                && !keys[0].starts_with("spoofed")
                && !keys[0].contains(&token),
            "{keys:?}"
        );

        // Revoking the browser from any client closes its socket within a
        // tick and its token stops validating.
        let (mut revoker, _) = Browser::hello(addr, hello(&token, "Chrome")).await;
        revoker
            .send(json!({"id":2,"payload":{"type":"query","content":{"type":"hosting","content":{"action":{"type":"revoke_device","content":browser_id}}}}}))
            .await;
        let started = Instant::now();
        assert!(browser.closed_within(Duration::from_secs(15)).await);
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "within one tick"
        );
        assert!(revoker.closed_within(Duration::from_secs(15)).await);
        assert_eq!(
            hosting_calls.lock().unwrap().as_slice(),
            ["State", "State"],
            "a browser device is revoked by the listener, not the native host"
        );
        let (_, rejected) = Browser::hello(addr, hello(&token, "Chrome")).await;
        assert_eq!(rejected["reason"], "token");
    });
    server.shutdown();
}

#[test]
fn two_browsers_get_their_own_acks_and_shared_broadcasts() {
    let dir = TestDir::new("two");
    let server = serve(fake_host().0, config(dir.0.clone())).unwrap();
    let addr = server.local_addr();
    setup(&server, "secret password");
    let tokens: Vec<String> = ["Chrome", "Firefox"]
        .into_iter()
        .map(|name| {
            login(addr, "secret password", name).1["token"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    tcode_traverse::block_on(async {
        let (mut one, _) = Browser::hello(addr, hello(&tokens[0], "Chrome")).await;
        let (mut two, _) = Browser::hello(addr, hello(&tokens[1], "Firefox")).await;
        for (browser, id) in [(&mut one, 7), (&mut two, 7)] {
            browser
                .send(json!({"id":id,"payload":{"type":"subscribe","content":{"topic":"index"}}}))
                .await;
            let mut kinds = vec![browser.recv().await["type"].as_str().unwrap().to_owned()];
            kinds.push(browser.recv().await["type"].as_str().unwrap().to_owned());
            kinds.sort();
            assert_eq!(kinds, ["ack", "event"]);
        }
        // The second subscription's snapshot is broadcast to the first too.
        assert_eq!(one.recv().await["type"], "event");
        one.send(json!({"id":8,"payload":{"type":"command","content":{"type":"create_project","content":{"path":"/tmp/shared"}}}}))
            .await;
        let mut seen = [one.recv().await, one.recv().await];
        seen.sort_by_key(|value| value["type"].as_str().unwrap().to_owned());
        assert_eq!(seen[0]["type"], "ack");
        assert_eq!(
            seen[0]["content"]["id"], 8,
            "acks carry the browser's own id"
        );
        assert_eq!(seen[1]["content"]["event"]["type"], "project_created");
        let broadcast = two.recv().await;
        assert_eq!(
            broadcast["content"]["event"]["type"], "project_created",
            "a subscribed browser sees the other's broadcast"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(300), two.0.next())
                .await
                .is_err(),
            "the ack reaches only its sender"
        );
    });
    server.shutdown();
}

#[test]
fn a_bind_beyond_loopback_needs_a_password_first() {
    let dir = TestDir::new("lan");
    let (mux, _) = fake_host();
    let lan = || BrowserConfig {
        listen: "0.0.0.0:0".parse().unwrap(),
        ..config(dir.0.clone())
    };
    let Err(error) = serve(mux.clone(), lan()) else {
        panic!("a LAN bind without a password must be refused");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("--password"), "{error}");
    assert!(
        !dir.0.join("remote.json").exists(),
        "a refused bind leaves nothing behind"
    );
    let early = tcode_traverse::browser::check_bind(lan().listen, &dir.0);
    assert_eq!(
        early.as_ref().map_err(|error| error.kind()),
        Err(std::io::ErrorKind::InvalidInput),
        "the same rule answers before anything is started"
    );
    set_password(&dir.0, "secret password", false).unwrap();
    tcode_traverse::browser::check_bind(lan().listen, &dir.0).unwrap();
    let server = serve(mux, lan()).unwrap();
    assert!(server.password_configured());
    server.shutdown();
}
