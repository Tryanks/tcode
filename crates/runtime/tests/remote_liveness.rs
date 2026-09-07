//! Real host pipe and HTTP/WebSocket transport, including a stopped host process.
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tcode_client::{ConnectionFailure, ConnectionState};
use tcode_remote::client::{RemoteClient, connect, pair};
use tcode_remote::{HostMux, RemoteConfig, serve};
use tcode_runtime::pipe::{HostServices, spawn_host};
use tcode_services::store::SessionStore;

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tcode-liveness-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn server(data: &TestDir) -> tcode_remote::RemoteServer {
    let host = spawn_host(
        SessionStore::open_at(data.0.clone()).unwrap(),
        HostServices::default(),
    )
    .unwrap();
    serve(
        HostMux::new(host.to_host, host.from_host),
        RemoteConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            host_name: "liveness test".into(),
            data_dir: data.0.clone(),
            static_bundle: None,
        },
    )
    .unwrap()
}

fn wait(
    client: &RemoteClient,
    timeout: Duration,
    predicate: impl Fn(&ConnectionState) -> bool,
) -> ConnectionState {
    smol::block_on(smol::future::race(
        async {
            loop {
                let state = client.state.recv().await.expect("state stream ended");
                if predicate(&state) {
                    return state;
                }
            }
        },
        async {
            smol::Timer::after(timeout).await;
            panic!("connection state deadline exceeded");
        },
    ))
}

fn ping(client: &RemoteClient) {
    client
        .to_host
        .send_blocking(
            "{\"id\":1,\"payload\":{\"type\":\"query\",\"content\":{\"type\":\"ping\"}}}".into(),
        )
        .unwrap();
}

#[test]
fn races_stalled_address_and_applies_first_host_message() {
    let data = TestDir::new();
    let server = server(&data);
    let code = server.new_pairing_code();
    let mut host = pair(
        &format!("http://127.0.0.1:{}", server.local_addr().port()),
        &code.code,
        "race",
    )
    .unwrap();
    // TCP connects, but the WebSocket upgrade cannot complete because this listener never accepts.
    // Unlike an unroutable IP, this deterministically stalls on every CI network.
    let _stalled =
        std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, server.local_addr().port()))
            .unwrap();
    host.origin = format!("http://localhost:{}", server.local_addr().port());
    let start = Instant::now();
    let client = connect(host, "race".into());
    wait(&client, Duration::from_secs(2), |s| {
        *s == ConnectionState::Syncing
    });
    assert!(client.state.try_recv().is_err());
    ping(&client);
    wait(&client, Duration::from_secs(2), |s| {
        *s == ConnectionState::Connected
    });
    assert!(start.elapsed() < Duration::from_secs(2));
    let reply = client.from_host.recv_blocking().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap(),
        serde_json::json!({"type":"query_result","content":{"id":1,"result":{"Ok":{"type":"pong"}}}})
    );
    client.to_host.close();
    server.shutdown();
}

#[test]
fn rejected_token_is_terminal_without_retry() {
    let data = TestDir::new();
    let server = server(&data);
    let code = server.new_pairing_code();
    let mut host = pair(
        &format!("http://127.0.0.1:{}", server.local_addr().port()),
        &code.code,
        "reject",
    )
    .unwrap();
    host.token = "invalid".into();
    let client = connect(host, "reject".into());
    wait(&client, Duration::from_secs(2), |s| {
        *s == ConnectionState::Offline {
            reason: ConnectionFailure::AuthenticationRejected,
        }
    });
    // Closure proves the attempt owner exited, rather than merely waiting out backoff.
    assert!(client.state.recv_blocking().is_err());
    assert!(client.from_host.recv_blocking().is_err());
    server.shutdown();
}

#[cfg(unix)]
#[test]
#[ignore = "child-process fixture, launched by stopped_host_times_out"]
fn child_host() {
    let data = TestDir(PathBuf::from(
        std::env::var_os("TCODE_LIVENESS_CHILD").expect("child fixture directory"),
    ));
    let server = server(&data);
    let code = server.new_pairing_code();
    std::fs::write(
        data.0.join("ready.json"),
        serde_json::to_vec(&code).unwrap(),
    )
    .unwrap();
    std::thread::park();
}

#[cfg(unix)]
#[test]
fn stopped_host_times_out() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let data = TestDir::new();
    let child = Child(
        tcode_services::process::command(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "child_host", "--nocapture"])
            .env("TCODE_LIVENESS_CHILD", &data.0)
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    let ready = data.0.join("ready.json");
    while !ready.exists() {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    let code: tcode_remote::server::PairingCode =
        serde_json::from_slice(&std::fs::read(ready).unwrap()).unwrap();
    let host = pair(
        &format!("http://127.0.0.1:{}", code.port),
        &code.code,
        "stop",
    )
    .unwrap();
    let client = connect(host, "stop".into());
    ping(&client);
    wait(&client, Duration::from_secs(2), |s| {
        *s == ConnectionState::Connected
    });
    client.from_host.recv_blocking().unwrap();
    // Put the last inbound frame just before the stop, exercising the full silence window.
    let start = Instant::now();
    assert!(
        tcode_services::process::command("kill")
            .args(["-STOP", &child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let state = wait(&client, Duration::from_millis(30_500), |s| {
        matches!(
            s,
            ConnectionState::Reconnecting {
                reason: Some(_),
                ..
            }
        )
    });
    let elapsed = start.elapsed();
    eprintln!(
        "SIGSTOP → Reconnecting: {:.3} s; {state:?}",
        elapsed.as_secs_f64()
    );
    assert_eq!(
        state,
        ConnectionState::Reconnecting {
            attempt: 1,
            reason: Some(ConnectionFailure::Timeout)
        }
    );
    // Allow scheduler/observation overhead at the specified 10 + 20 second boundary.
    assert!(elapsed < Duration::from_millis(30_250));
    client.to_host.close();
}
