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
            },
        )
        .unwrap();
        let host = tcode_remote::client::pair(
            &format!("http://{}", server.local_addr()),
            &server.new_pairing_code().code,
            "test device",
        )
        .unwrap();
        Self {
            server: Some(server),
            root,
            auth: format!(
                "Proxy-Authorization: Basic {}\r\n",
                STANDARD.encode(format!("tcode:{}", host.token))
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
fn http_get_resolves_loopback_on_machine_and_preserves_query_without_leaking_credentials() {
    let machine = Machine::new();
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
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate).unwrap();
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut tls = rustls::StreamOwned::new(
        rustls::ClientConnection::new(Arc::new(config), ServerName::try_from("localhost").unwrap())
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
