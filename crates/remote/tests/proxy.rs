//! Exercise the public listener and real pairing, not a second proxy implementation.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tcode_remote::{HostMux, RemoteConfig, RemoteServer, serve};

struct Machine {
    server: Option<RemoteServer>,
    root: PathBuf,
    auth: String,
}
impl Machine {
    fn new() -> Self {
        Self::with_password(false)
    }
    fn with_password(password: bool) -> Self {
        let root = std::env::temp_dir().join(format!("tcode-proxy-{}", uuid::Uuid::new_v4()));
        let (to_host, _) = async_channel::unbounded();
        let (_, from_host) = async_channel::unbounded();
        let server = serve(
            HostMux::new(to_host, from_host),
            RemoteConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                host_name: "proxy test".into(),
                data_dir: root.clone(),
                static_bundle: None,
                browser_password: password,
            },
        )
        .unwrap();
        let origin = format!("http://{}", server.local_addr());
        let token = if password {
            let body = serde_json::json!({"password":"proxy password","device_name":"browser"})
                .to_string();
            tcode_remote::client::http(&origin, "POST", "/auth/setup", &body).unwrap();
            let bytes = tcode_remote::client::http(&origin, "POST", "/auth/login", &body).unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["token"]
                .as_str()
                .unwrap()
                .to_owned()
        } else {
            tcode_remote::client::pair(&origin, &server.new_pairing_code().code, "test device")
                .unwrap()
                .token
        };
        Self {
            server: Some(server),
            root,
            auth: format!(
                "Proxy-Authorization: Basic {}\r\n",
                STANDARD.encode(format!("tcode:{}", token))
            ),
        }
    }
    fn socket(&self) -> TcpStream {
        let socket = TcpStream::connect(self.server.as_ref().unwrap().local_addr()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        socket
    }
}
impl Drop for Machine {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown();
        }
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
fn head(stream: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() <= 16384);
    }
    String::from_utf8(bytes).unwrap()
}

#[test]
fn password_token_proxies_loopback_and_preserves_query_without_leaking_credentials() {
    let machine = Machine::with_password(true);
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = origin.local_addr().unwrap().port();
    let origin = std::thread::spawn(move || {
        let (mut stream, _) = origin.accept().unwrap();
        let request = head(&mut stream);
        assert!(request.starts_with("GET /page?source=host HTTP/1.1\r\n"));
        assert!(!request.to_ascii_lowercase().contains("proxy-authorization"));
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\nserved on host",
            )
            .unwrap();
    });
    let mut socket = machine.socket();
    socket.write_all(format!("GET http://localhost:{port}/page?source=host HTTP/1.1\r\nHost: wrong.example\r\n{}\r\n", machine.auth).as_bytes()).unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with("served on host"));
    origin.join().unwrap();
}

#[test]
fn missing_invalid_and_revoked_credentials_receive_407_for_http_and_connect() {
    let machine = Machine::new();
    for method in ["GET http://localhost:1/", "CONNECT localhost:1"] {
        for auth in ["", "Proxy-Authorization: Basic dGNvZGU6aW52YWxpZA==\r\n"] {
            let mut socket = machine.socket();
            socket
                .write_all(
                    format!("{method} HTTP/1.1\r\nHost: localhost:1\r\n{auth}\r\n").as_bytes(),
                )
                .unwrap();
            let response = head(&mut socket);
            assert!(response.starts_with("HTTP/1.1 407 "));
            assert!(response.contains("Proxy-Authenticate: Basic realm=\"tcode-preview\""));
        }
    }
    let server = machine.server.as_ref().unwrap();
    server.revoke_device(&server.devices()[0].id).unwrap();
    let mut socket = machine.socket();
    socket
        .write_all(format!("CONNECT localhost:1 HTTP/1.1\r\n{}\r\n", machine.auth).as_bytes())
        .unwrap();
    assert!(head(&mut socket).starts_with("HTTP/1.1 407 "));
}

#[test]
fn connect_carries_verified_tls_bytes_without_interception() {
    for forwarded in [false, true] {
        use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};
        let machine = Machine::new();
        // This self-signed localhost identity is trusted only by this test client.
        let certificate = CertificateDer::from(include_bytes!("fixtures/localhost.der").to_vec());
        let key = PrivatePkcs8KeyDer::from(include_bytes!("fixtures/localhost-key.der").to_vec());
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key.into())
            .unwrap();
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = origin.local_addr().unwrap().port();
        let origin = std::thread::spawn(move || {
            let (stream, _) = origin.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut tls = rustls::StreamOwned::new(
                rustls::ServerConnection::new(Arc::new(server_config)).unwrap(),
                stream,
            );
            assert!(head(&mut tls).starts_with("GET /secure HTTP/1.1"));
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nsecure")
                .unwrap();
            tls.conn.send_close_notify();
            tls.flush().unwrap();
        });
        let mut routes = paired_preview(&machine);
        let socket = if forwarded {
            let intent = format!("https://localhost:{port}/secure");
            let actual = routes.navigate(&intent).unwrap();
            assert_eq!(routes.logical_url(&actual), intent);
            let port = url::Url::parse(&actual).unwrap().port().unwrap();
            let socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            socket
        } else {
            let mut socket = machine.socket();
            socket
                .write_all(
                    format!(
                        "CONNECT localhost:{port} HTTP/1.1\r\nHost: localhost:{port}\r\n{}\r\n",
                        machine.auth
                    )
                    .as_bytes(),
                )
                .unwrap();
            assert!(head(&mut socket).starts_with("HTTP/1.1 200 Connection Established"));
            socket
        };
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate).unwrap();
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut tls = rustls::StreamOwned::new(
            rustls::ClientConnection::new(
                Arc::new(config),
                ServerName::try_from("localhost").unwrap(),
            )
            .unwrap(),
            socket,
        );
        tls.write_all(b"GET /secure HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        assert!(head(&mut tls).starts_with("HTTP/1.1 200 OK"));
        let mut body = [0; 6];
        tls.read_exact(&mut body).unwrap();
        assert_eq!(&body, b"secure");
        origin.join().unwrap();
    }
}
#[test]
fn pipelined_request_and_chunk_trailers_never_reach_origin() {
    let machine = Machine::new();
    for chunked in [false, true] {
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = origin.local_addr().unwrap().port();
        let origin = std::thread::spawn(move || {
            let (mut stream, _) = origin.accept().unwrap();
            head(&mut stream);
            let expected = if chunked {
                b"3\r\nabc\r\n0\r\n\r\n".as_slice()
            } else {
                b"abc".as_slice()
            };
            let mut body = vec![0; expected.len()];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(body, expected);
            stream
                .set_read_timeout(Some(Duration::from_millis(150)))
                .unwrap();
            assert!(
                stream.read(&mut [0; 1]).is_err(),
                "pipelined bytes leaked to origin"
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let framing = if chunked {
            "Transfer-Encoding: chunked"
        } else {
            "Content-Length: 3"
        };
        let body = if chunked {
            "3\r\nabc\r\n0\r\nProxy-Authorization: secret\r\n\r\n"
        } else {
            "abc"
        };
        let mut socket = machine.socket();
        socket.write_all(format!("POST http://localhost:{port}/ HTTP/1.1\r\n{framing}\r\n{}\r\n{body}GET http://evil/ HTTP/1.1\r\n{}\r\n", machine.auth, machine.auth).as_bytes()).unwrap();
        assert!(head(&mut socket).starts_with("HTTP/1.1 200 OK"));
        origin.join().unwrap();
    }
}

#[test]
fn connect_preserves_half_close_and_host_shutdown_closes_active_tunnels() {
    let mut machine = Machine::new();
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = origin.local_addr().unwrap().port();
    let origin = std::thread::spawn(move || {
        let (mut stream, _) = origin.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).unwrap();
        assert_eq!(body, "request");
        stream.write_all(b"reply after EOF").unwrap();
    });
    let mut socket = machine.socket();
    socket
        .write_all(format!("CONNECT localhost:{port} HTTP/1.1\r\n{}\r\n", machine.auth).as_bytes())
        .unwrap();
    assert!(head(&mut socket).starts_with("HTTP/1.1 200"));
    socket.write_all(b"request").unwrap();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    let mut body = String::new();
    socket.read_to_string(&mut body).unwrap();
    assert_eq!(body, "reply after EOF");
    origin.join().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut socket = machine.socket();
    socket
        .write_all(
            format!(
                "CONNECT {} HTTP/1.1\r\n{}\r\n",
                listener.local_addr().unwrap(),
                machine.auth
            )
            .as_bytes(),
        )
        .unwrap();
    assert!(head(&mut socket).starts_with("HTTP/1.1 200"));
    let (_target, _) = listener.accept().unwrap();
    machine.server.take().unwrap().shutdown();
    assert_eq!(socket.read(&mut [0; 1]).unwrap(), 0);
}

fn paired_preview(machine: &Machine) -> tcode_remote::preview::PreviewRoutes {
    let encoded = machine
        .auth
        .trim()
        .strip_prefix("Proxy-Authorization: Basic ")
        .unwrap();
    let decoded = String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap();
    tcode_remote::preview::PreviewRoutes::new(tcode_remote::client::PairedHost {
        host_id: "fixture".into(),
        name: "fixture".into(),
        origin: format!("http://{}", machine.server.as_ref().unwrap().local_addr()),
        token: decoded.strip_prefix("tcode:").unwrap().into(),
        last_connected_unix: None,
    })
}

#[test]
fn browser_forward_routes_bytes_and_navigation_then_disposes_connections() {
    let machine = Machine::new();
    for (first, next) in [
        (
            "http://localhost/",
            "https://localhost:80/page?scheme=https",
        ),
        (
            "https://localhost/",
            "http://localhost:443/page?scheme=http",
        ),
    ] {
        let mut routes = paired_preview(&machine);
        let initial = url::Url::parse(&routes.navigate(first).unwrap()).unwrap();
        let actual = routes.navigate(next).unwrap();
        assert_eq!(
            url::Url::parse(&actual).unwrap().port(),
            initial.port(),
            "changing scheme retains the same TCP route"
        );
        assert_eq!(routes.logical_url(&actual), next);
        assert_eq!(routes.navigation(&actual).unwrap(), actual);
        assert_eq!(routes.current_url(), Some(next));
        assert_eq!(routes.external_url(next).as_deref(), Some(actual.as_str()));
    }
    // The viewing machine already occupies the remote port. The mapped socket
    // must be allocated elsewhere, including when a second browser opens it.
    let destination = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = destination.local_addr().unwrap().port();
    let intent = format!("http://localhost:{port}/page?source=remote#fragment");
    let mut routes = paired_preview(&machine);
    let actual = routes.navigate(&intent).unwrap();
    assert_ne!(
        actual, intent,
        "remote navigation must allocate a viewing endpoint"
    );
    let actual_url = url::Url::parse(&actual).unwrap();
    assert_ne!(actual_url.port().unwrap(), port);
    assert_eq!(routes.logical_url(&actual), intent);
    assert_eq!(
        routes.navigate(&intent).unwrap(),
        actual,
        "reload reuses the route"
    );
    assert_eq!(
        routes.navigation(&actual).unwrap(),
        actual,
        "native reentry is not mapped twice"
    );
    let entered_collision = format!(
        "http://localhost:{}/another-remote-port",
        actual_url.port().unwrap()
    );
    assert_ne!(
        routes.navigate(&entered_collision).unwrap(),
        entered_collision,
        "explicit remote intent must not be mistaken for native reentry"
    );
    let public = "https://example.com/public?viewer=direct";
    assert_eq!(routes.navigate(public).unwrap(), public);
    assert_eq!(routes.navigation(&actual).unwrap(), actual);
    assert_eq!(
        routes.external_url(&intent).as_deref(),
        Some(actual.as_str())
    );
    let mut second = paired_preview(&machine);
    assert_ne!(second.navigate(&intent).unwrap(), actual);
    let mut browser = TcpStream::connect(("127.0.0.1", actual_url.port().unwrap())).unwrap();
    browser
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // Opaque HTTP/WS-like bytes, including site authorization and binary bytes:
    // the CONNECT client must not parse/rewrite/remove any of these.
    let bytes = b"GET /page?source=remote HTTP/1.1\r\nHost: localhost:12345\r\nAuthorization: Basic site-only\r\nUpgrade: websocket\r\n\r\n\x00\xff\x81payload";
    browser.write_all(bytes).unwrap();
    let (mut remote, _) = destination.accept().unwrap();
    remote
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut received = vec![0; bytes.len()];
    remote.read_exact(&mut received).unwrap();
    assert_eq!(received, bytes);
    browser.shutdown(std::net::Shutdown::Write).unwrap();
    assert_eq!(remote.read(&mut [0]).unwrap(), 0);
    remote.write_all(b"remote after EOF").unwrap();
    let mut reply = [0; 16];
    browser.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"remote after EOF");
    drop(routes);
    assert_eq!(
        browser.read(&mut [0]).unwrap(),
        0,
        "slot close must close accepted tunnels"
    );
    assert_eq!(remote.read(&mut [0]).unwrap(), 0);
}

#[test]
fn browser_forward_uses_paired_auth_and_preserves_ipv6_destination() {
    let machine = Machine::new();
    let destination = TcpListener::bind("[::1]:0").unwrap();
    let intent = format!(
        "http://[::1]:{}/ipv6",
        destination.local_addr().unwrap().port()
    );
    let mut routes = paired_preview(&machine);
    let actual = routes.navigate(&intent).unwrap();
    let actual_url = url::Url::parse(&actual).unwrap();
    assert_eq!(actual_url.host_str(), Some("[::1]"));
    let mut browser = TcpStream::connect(("::1", actual_url.port().unwrap())).unwrap();
    browser
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    browser.write_all(b"IPv6 remote").unwrap();
    let (mut remote, _) = destination.accept().unwrap();
    remote
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = [0; 11];
    remote.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"IPv6 remote");
    drop(browser);
    drop(remote);
    drop(destination);
    let mut browser = TcpStream::connect(("::1", actual_url.port().unwrap())).unwrap();
    browser
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(browser.read(&mut [0]).unwrap(), 0);
    assert!(routes.error().unwrap().contains("destination"));
    routes.navigation(&actual).unwrap();
    assert_eq!(
        routes.error(),
        None,
        "native reload clears the previous attempt's error"
    );
    let server = machine.server.as_ref().unwrap();
    server.revoke_device(&server.devices()[0].id).unwrap();
    let mut browser = TcpStream::connect(("::1", actual_url.port().unwrap())).unwrap();
    browser
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(browser.read(&mut [0]).unwrap(), 0);
    assert!(routes.error().unwrap().contains("authentication"));
}

#[test]
fn browser_top_level_cross_port_navigation_reaches_the_new_remote_listener() {
    let machine = Machine::new();
    let first = TcpListener::bind("127.0.0.1:0").unwrap();
    let next = TcpListener::bind("[::1]:0").unwrap();
    let mut routes = paired_preview(&machine);
    let initial = format!(
        "http://localhost:{}/start",
        first.local_addr().unwrap().port()
    );
    routes.navigate(&initial).unwrap();
    // An absolute Location header carries a new remote authority. It enters
    // the same production navigation owner used by the WK delegate.
    let redirect = format!(
        "http://[::1]:{}/arrived?from=redirect",
        next.local_addr().unwrap().port()
    );
    let actual = routes.navigation(&redirect).unwrap();
    assert_eq!(routes.logical_url(&actual), redirect);
    assert_eq!(routes.current_url(), Some(redirect.as_str()));
    let url = url::Url::parse(&actual).unwrap();
    let mut browser = TcpStream::connect(("::1", url.port().unwrap())).unwrap();
    browser
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    browser
        .write_all(b"GET /arrived?from=redirect HTTP/1.1\r\nHost: fixture\r\n\r\n")
        .unwrap();
    let (mut remote, _) = next.accept().unwrap();
    remote
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert!(head(&mut remote).starts_with("GET /arrived?from=redirect HTTP/1.1"));
    remote.write_all(b"second remote port").unwrap();
    let mut body = [0; 18];
    browser.read_exact(&mut body).unwrap();
    assert_eq!(&body, b"second remote port");
    drop(routes);
    assert_eq!(browser.read(&mut [0]).unwrap(), 0);
}

#[test]
fn browser_keepalive_shutdown_does_not_report_a_failed_navigation() {
    let machine = Machine::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut routes = paired_preview(&machine);
    let actual = routes
        .navigate(&format!(
            "http://localhost:{}/",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
    let port = url::Url::parse(&actual).unwrap().port().unwrap();
    let mut browser = TcpStream::connect(("127.0.0.1", port)).unwrap();
    browser
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    browser
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let (mut remote, _) = listener.accept().unwrap();
    remote
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    head(&mut remote);
    remote
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
        .unwrap();
    drop(remote);
    let mut response = String::new();
    browser.read_to_string(&mut response).unwrap();
    assert!(response.ends_with("ok"));
    drop(browser);
    // The task can finish after the reader gets EOF. A subsequent round trip
    // through the same listener lets the executor process that completion.
    let mut browser = TcpStream::connect(("127.0.0.1", port)).unwrap();
    browser
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (mut remote, _) = listener.accept().unwrap();
    remote.write_all(b"next").unwrap();
    browser.read_exact(&mut [0; 4]).unwrap();
    assert_eq!(routes.error(), None);
}

#[test]
fn native_bridge_preserves_reverse_half_close_auth_and_attachment_lifetime() {
    for bridged in [false, true] {
        let machine = Machine::new();
        let server = machine.server.as_ref().unwrap();
        let host = tcode_remote::client::PairedHost {
            origin: format!("http://{}", server.local_addr()),
            host_id: String::new(),
            name: String::new(),
            token: String::new(),
            last_connected_unix: None,
        };
        let bridge = bridged.then(|| tcode_remote::preview::NativeProxy::new(&host).unwrap());
        let address = bridge
            .as_ref()
            .map(|bridge| bridge.origin().trim_start_matches("http://").to_owned())
            .unwrap_or_else(|| server.local_addr().to_string());
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let destination = target.local_addr().unwrap();
        let target = std::thread::spawn(move || {
            let (mut target, _) = target.accept().unwrap();
            target
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            target.write_all(b"response first").unwrap();
            target.shutdown(std::net::Shutdown::Write).unwrap();
            let mut request = String::new();
            target.read_to_string(&mut request).unwrap();
            assert_eq!(request, "request after remote EOF");
        });
        let mut browser = TcpStream::connect(&address).unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        browser
            .write_all(format!("CONNECT {destination} HTTP/1.1\r\n{}\r\n", machine.auth).as_bytes())
            .unwrap();
        assert!(head(&mut browser).starts_with("HTTP/1.1 200 "));
        let mut reply = String::new();
        browser.read_to_string(&mut reply).unwrap();
        assert_eq!(reply, "response first");
        browser.write_all(b"request after remote EOF").unwrap();
        browser.shutdown(std::net::Shutdown::Write).unwrap();
        target.join().unwrap();
        if let Some(bridge) = bridge {
            let mut unauthorized = TcpStream::connect(&address).unwrap();
            unauthorized
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            unauthorized
                .write_all(b"CONNECT localhost:1 HTTP/1.1\r\n\r\n")
                .unwrap();
            assert!(head(&mut unauthorized).starts_with("HTTP/1.1 407 "));
            let mut pending = TcpStream::connect(&address).unwrap();
            pending
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            pending.write_all(b"GET ").unwrap();
            drop(bridge);
            match pending.read(&mut [0]) {
                Ok(0) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                result => panic!("bridge did not close its pending connection: {result:?}"),
            }
        }
    }
}

#[test]
fn admitted_pending_streams_cancel_and_stop_without_retaining_server() {
    smol::block_on(async {
        use futures_lite::io::{AsyncReadExt as _, AsyncWriteExt as _};
        for shutdown in [false, true] {
            let mut machine = Machine::new();
            let listener = smol::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut peer = smol::net::TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let admission = machine.server.as_ref().unwrap().admit(stream);
            let mut admission = Box::pin(admission);
            assert!(
                futures_lite::future::poll_once(&mut admission)
                    .await
                    .is_none()
            );
            peer.write_all(b"GET /admin").await.unwrap();
            assert!(
                futures_lite::future::poll_once(&mut admission)
                    .await
                    .is_none()
            );
            if shutdown {
                machine.server.take().unwrap().shutdown();
                admission.await.unwrap();
            } else {
                drop(admission);
            }
            futures_lite::future::race(
                async {
                    match peer.read(&mut [0]).await {
                        Ok(0) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                            ) => {}
                        result => panic!("admitted stream did not close: {result:?}"),
                    }
                },
                async {
                    smol::Timer::after(Duration::from_secs(2)).await;
                    panic!("admitted stream survived cancellation or shutdown");
                },
            )
            .await;
        }
    });
}
