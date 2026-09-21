//! The browser listener: static bundle, password login and the `/ws` hello.
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tcode_remote::{HostMux, RemoteConfig, serve};

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

/// Acks every command and answers every subscription with an index event.
fn fake_host() -> HostMux {
    let (to_host, host_rx) = async_channel::unbounded::<String>();
    let (host_tx, from_host) = async_channel::unbounded::<String>();
    std::thread::spawn(move || {
        while let Ok(line) = host_rx.recv_blocking() {
            let value: Value = serde_json::from_str(line.trim_end()).unwrap();
            let id = value["id"].as_u64().unwrap();
            if value["payload"]["type"] == "subscribe" {
                host_tx
                    .send_blocking(
                        json!({"type": "event", "content": {"topic": "index", "event": {"type": "index_snapshot", "content": {"sessions": [], "projects": []}}}})
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
    HostMux::new(to_host, from_host)
}

fn config(data_dir: PathBuf) -> RemoteConfig {
    RemoteConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        host_name: "Test Host".into(),
        data_dir,
        static_bundle: None,
        browser_password: false,
        hosting: None,
    }
}

/// One bounded HTTP/1.1 request; returns the status line and body.
fn http(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> (String, Vec<u8>) {
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

#[test]
fn static_bundle_get_and_head_share_headers() {
    let dir = TestDir::new();
    let mut config = config(dir.0.clone());
    config.static_bundle = Some(&[
        ("/index.html", b"<html>tcode</html>"),
        ("/tcode_web_bg.wasm", b"\0asm"),
    ]);
    let server = serve(fake_host(), config).unwrap();
    for (path, status, content_type, length) in [
        ("/", "200 OK", "text/html; charset=utf-8", 18),
        ("/index.html", "200 OK", "text/html; charset=utf-8", 18),
        ("/tcode_web_bg.wasm", "200 OK", "application/wasm", 4),
        ("/missing", "404 Not Found", "text/plain; charset=utf-8", 9),
    ] {
        let request = |method| {
            let stream = TcpStream::connect(server.local_addr()).unwrap();
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
fn browser_password_login_hello_hosting_query_and_lockout() {
    let root = TestDir::new();
    let mut config = config(root.0.clone());
    config.browser_password = true;
    let hosting_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = hosting_calls.clone();
    config.hosting = Some(Arc::new(move |action| {
        seen.lock().unwrap().push(format!("{action:?}"));
        tcode_protocol::HostingState {
            enabled: true,
            code: Some("123456".into()),
            expires_in_secs: 299,
            host_id: "machine".into(),
            host_name: "Test Host".into(),
            invite: None,
            devices: Vec::new(),
        }
    }));
    let server = serve(fake_host(), config).unwrap();
    let addr = server.local_addr();
    let (status, body) = http(addr, "GET", "/auth/state", "");
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"mode":"password","configured":false})
    );
    assert!(
        http(addr, "POST", "/auth/setup", r#"{"password":"short"}"#)
            .0
            .contains("400")
    );
    let password = json!({"password":"secret password"}).to_string();
    assert_eq!(
        http(addr, "POST", "/auth/setup", &password).0,
        "HTTP/1.1 200 OK"
    );
    assert!(
        http(addr, "POST", "/auth/setup", &password)
            .0
            .contains("409")
    );
    assert!(server.password_configured());
    let login = json!({"password":"secret password","device_name":"Browser"}).to_string();
    let (status, body) = http(addr, "POST", "/auth/login", &login);
    assert_eq!(status, "HTTP/1.1 200 OK");
    let paired: Value = serde_json::from_slice(&body).unwrap();
    let token = paired["token"].as_str().unwrap().to_owned();
    assert_eq!(paired["host_name"], "Test Host");
    // The retired native paths are gone from the listener.
    for (method, path) in [
        ("POST", "/pair"),
        ("GET", "/admin/pair"),
        ("POST", "/identity"),
    ] {
        assert_eq!(http(addr, method, path, "{}").0, "HTTP/1.1 404 Not Found");
    }

    let socket = TcpStream::connect(addr).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let (mut ws, _) = tungstenite::client(format!("ws://{addr}/ws"), socket).unwrap();
    let send = |ws: &mut tungstenite::WebSocket<TcpStream>, value: Value| {
        ws.send(tungstenite::Message::Text(value.to_string().into()))
            .unwrap();
    };
    let recv = |ws: &mut tungstenite::WebSocket<TcpStream>| -> Value {
        loop {
            if let tungstenite::Message::Text(text) = ws.read().unwrap() {
                return serde_json::from_str(&text).unwrap();
            }
        }
    };
    send(
        &mut ws,
        json!({"type":"hello","protocol_version":3,"supported_versions":[3,4],"token":token,"device_name":"Browser"}),
    );
    let hello_ok = recv(&mut ws);
    assert_eq!(hello_ok["type"], "hello_ok");
    assert_eq!(hello_ok["protocol_version"], 4);
    assert_eq!(hello_ok["host_name"], "Test Host");
    send(
        &mut ws,
        json!({"id":900,"payload":{"type":"query","content":{"type":"hosting","content":{"action":{"type":"state"}}}}}),
    );
    let reply = recv(&mut ws);
    assert_eq!(reply["type"], "query_result");
    assert_eq!(reply["content"]["id"], 900);
    assert_eq!(
        reply["content"]["result"]["Ok"]["content"]["code"], "123456",
        "hosting queries are answered by the handler the host installed"
    );
    assert_eq!(hosting_calls.lock().unwrap().as_slice(), ["State"]);
    send(
        &mut ws,
        json!({"id":1,"payload":{"type":"subscribe","content":{"topic":"index"}}}),
    );
    let mut kinds = vec![recv(&mut ws)["type"].as_str().unwrap().to_owned()];
    kinds.push(recv(&mut ws)["type"].as_str().unwrap().to_owned());
    kinds.sort();
    assert_eq!(kinds, ["ack", "event"]);

    let socket = TcpStream::connect(addr).unwrap();
    let (mut rejected, _) = tungstenite::client(format!("ws://{addr}/ws"), socket).unwrap();
    send(
        &mut rejected,
        json!({"type":"hello","protocol_version":4,"token":"not-a-token","device_name":"Browser"}),
    );
    let reply = recv(&mut rejected);
    assert_eq!(reply["type"], "hello_rejected");
    assert_eq!(reply["reason"], "token");

    for _ in 0..5 {
        assert!(
            http(
                addr,
                "POST",
                "/auth/login",
                &json!({"password":"incorrect","device_name":"Browser"}).to_string()
            )
            .0
            .contains("403")
        );
    }
    assert!(
        http(addr, "POST", "/auth/login", &login).0.contains("403"),
        "five failures lock the right password out"
    );
    server.shutdown();
}
