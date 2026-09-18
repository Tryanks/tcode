use super::*;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

struct TestProfile(PathBuf);

impl TestProfile {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("tcode-network-move-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestProfile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn recovery_host() -> (crate::HostMux, Arc<Mutex<Vec<Value>>>) {
    let (requests, receiver) = async_channel::unbounded::<String>();
    let (events, replies) = async_channel::unbounded::<String>();
    let subscriptions = Arc::new(Mutex::new(Vec::new()));
    let observed = subscriptions.clone();
    std::thread::spawn(move || {
        while let Ok(line) = receiver.recv_blocking() {
            let request: Value = serde_json::from_str(&line).unwrap();
            if request["payload"]["type"] != "subscribe" {
                continue;
            }
            let topic = request["payload"]["content"]["topic"].clone();
            observed.lock().unwrap().push(topic.clone());
            let event = if topic["type"] == "index" {
                json!({"type":"index_snapshot", "content":{"sessions":[], "projects":[]}})
            } else {
                json!({"type":"session_snapshot", "content":{"from":0, "records":[], "total":0, "total_turns":0, "truncated":false}})
            };
            let snapshot = json!({"type":"event", "content":{"topic":topic, "event":event}});
            if events.send_blocking(snapshot.to_string()).is_err() {
                break;
            }
            let ack = json!({"type":"ack", "content":{"id":request["id"], "result":{"Ok":{"type":"unit"}}}});
            if events.send_blocking(ack.to_string()).is_err() {
                break;
            }
        }
    });
    (crate::HostMux::new(requests, replies), subscriptions)
}

fn recovery_config(profile: &TestProfile) -> crate::RemoteConfig {
    crate::RemoteConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        host_name: "Moving workstation".into(),
        data_dir: profile.0.clone(),
        static_bundle: None,
        browser_password: false,
    }
}

fn receive_baselines(incoming: &Receiver<String>) {
    smol::block_on(futures_lite::future::race(
        async {
            let mut topics = std::collections::HashSet::new();
            while topics.len() < 2 {
                let line = incoming
                    .recv()
                    .await
                    .expect("connection closed during recovery");
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["type"] == "event" {
                    // These are actual protocol snapshots forwarded through HostMux.
                    serde_json::from_value::<tcode_protocol::HostMessage>(value.clone()).unwrap();
                    topics.insert(
                        value["content"]["topic"]["type"]
                            .as_str()
                            .unwrap()
                            .to_owned(),
                    );
                }
            }
            assert!(topics.contains("index"));
            assert!(topics.contains("session_events"));
        },
        async {
            smol::Timer::after(Duration::from_secs(15)).await;
            panic!("network move did not rediscover the host and restore both subscriptions");
        },
    ));
}

#[test]
fn moving_to_an_unknown_lan_address_discovers_authenticates_and_restores_subscriptions() {
    const HOME: &str = "http://192.168.31.42:47420";
    const OFFICE: &str = "http://192.168.1.161:47420";
    let machine = TestProfile::new();
    let client_profile = TestProfile::new();
    let (mux, subscriptions) = recovery_host();
    let old_server = crate::serve(mux.clone(), recovery_config(&machine)).unwrap();
    let device = DeviceIdentity {
        id: "moving-phone".into(),
        name: "Moving phone".into(),
        platform: None,
    };
    let code = old_server.new_pairing_code();
    let mut host = pair(
        &format!("http://{}", old_server.local_addr()),
        &code.code,
        &device,
    )
    .unwrap();
    host.origin = HOME.into();
    assert!(host.candidates.is_empty());
    let original = host.clone();
    save_hosts(&client_profile.0, std::slice::from_ref(&host)).unwrap();

    // The seam only routes TCP: discovery, identity verification, the retry
    // loop, persistence, and subscription replay are production code. Nothing
    // contacts the developer's LAN, and the new origin is never preseeded.
    let route = Arc::new(Mutex::new((HOME.to_owned(), old_server.local_addr())));
    let discovery_used = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let route_for_loop = route.clone();
    let discovery_for_loop = discovery_used.clone();
    let establish = move |host: PairedHost, device: DeviceIdentity, race: bool, round: u32| {
        let route = route_for_loop.clone();
        let discovered = discovery_for_loop.clone();
        async move {
            let moved = route.lock().unwrap().0 == OFFICE;
            let networks = [crate::discovery::LocalNetwork {
                name: "en0".into(),
                ip: if moved {
                    "192.168.1.22"
                } else {
                    "192.168.31.22"
                }
                .parse()
                .unwrap(),
                prefix: Some(24),
            }];
            let origins = recovery_origins(&host, race, round, &networks);
            if moved && host.origin != OFFICE {
                assert!(!host.candidates.iter().any(|origin| origin == OFFICE));
                if origins
                    .iter()
                    .any(|(origin, probe)| origin == OFFICE && *probe)
                {
                    discovered.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let host = &host;
            let device = &device;
            connect_origins(origins, move |origin, primary| {
                let route = route.clone();
                async move {
                    let address: SocketAddr = {
                        let route = route.lock().unwrap();
                        if route.0 != origin {
                            return Err(ConnectionFailure::Unreachable);
                        }
                        route.1
                    };
                    let stream = smol::net::TcpStream::connect(address)
                        .await
                        .map_err(|_| ConnectionFailure::Unreachable)?;
                    open_websocket(
                        Box::new(stream),
                        &url::Url::parse(&origin).unwrap(),
                        host,
                        device,
                        primary,
                    )
                    .await
                }
            })
            .await
        }
    };
    let (to_host, outgoing) = tcode_client::outgoing::channel();
    let (incoming, from_host) = async_channel::unbounded();
    let (state_tx, states) = async_channel::unbounded();
    let data_dir = client_profile.0.clone();
    let loop_thread = std::thread::spawn(move || {
        smol::block_on(connection_loop(
            LiveHost::new(host),
            device,
            Some(data_dir),
            outgoing,
            incoming,
            state_tx,
            establish,
        ));
    });
    let topics = [
        json!({"type":"index"}),
        json!({"type":"session_events", "content":{"session_id":"ongoing-thread"}}),
    ];
    for (index, topic) in topics.iter().enumerate() {
        to_host
            .send_blocking(
                json!({"id":index + 1, "payload":{"type":"subscribe", "content":{"topic":topic}}})
                    .to_string(),
            )
            .unwrap();
    }
    receive_baselines(&from_host);
    assert_eq!(subscriptions.lock().unwrap().len(), 2);
    assert!(
        !load_hosts(&client_profile.0).unwrap()[0]
            .candidates
            .iter()
            .any(|origin| origin == OFFICE)
    );

    // Keep the old listener alive until the new one binds, guaranteeing a
    // genuinely different endpoint while retaining the host profile and token.
    let new_server = crate::serve(mux, recovery_config(&machine)).unwrap();
    assert_ne!(old_server.local_addr(), new_server.local_addr());
    *route.lock().unwrap() = (OFFICE.into(), new_server.local_addr());
    old_server.shutdown();
    receive_baselines(&from_host);

    assert!(discovery_used.load(std::sync::atomic::Ordering::SeqCst));
    let observed = subscriptions.lock().unwrap();
    for topic in &topics {
        assert_eq!(observed.iter().filter(|seen| *seen == topic).count(), 2);
    }
    drop(observed);
    let saved = load_hosts(&client_profile.0).unwrap().remove(0);
    assert_eq!(saved.origin, OFFICE);
    assert_eq!(saved.host_id, original.host_id);
    assert_eq!(saved.token, original.token);
    assert_eq!(saved.identity_key, original.identity_key);
    assert_eq!(
        new_server.devices().len(),
        1,
        "recovery must reuse the pairing"
    );
    let mut syncs = 0;
    while let Ok(state) = states.try_recv() {
        assert!(
            !matches!(state, ConnectionState::Offline { .. }),
            "unexpected terminal failure: {state:?}"
        );
        syncs += usize::from(state == ConnectionState::Syncing);
    }
    assert_eq!(syncs, 2);
    to_host.close();
    loop_thread.join().unwrap();
    new_server.shutdown();
}

#[test]
fn an_endpoint_claiming_the_saved_host_id_never_receives_the_bearer_token() {
    use futures_lite::io::AsyncReadExt as _;
    use ring::signature::KeyPair as _;

    let real_key = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[1; 32]).unwrap();
    let pinned_key = crate::identity::encode_hex(real_key.public_key().as_ref());
    for pin in [Some(pinned_key), None] {
        smol::block_on(async {
            let listener = smol::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let host = PairedHost {
                host_id: "saved-workstation".into(),
                name: "Saved workstation".into(),
                origin: "http://192.168.1.161:47420".into(),
                token: "A".repeat(43),
                identity_key: pin,
                candidates: Vec::new(),
                last_connected_unix: None,
            };
            let device = DeviceIdentity {
                id: "phone".into(),
                name: "Phone".into(),
                platform: None,
            };
            let attacker = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = crate::wire::read_request(&mut stream).await.unwrap();
                assert_eq!(request.method, "POST");
                assert_eq!(request.path, "/identity");
                assert!(
                    !request
                        .headers
                        .values()
                        .any(|value| value.contains(&host.token))
                );
                let request: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(request["type"], "identify");
                assert!(request.get("token").is_none());
                let challenge: crate::identity::IdentityChallenge =
                    serde_json::from_value(request).unwrap();
                let attacker_key =
                    ring::signature::Ed25519KeyPair::from_seed_unchecked(&[2; 32]).unwrap();
                // The signature is valid, and host_id/nonce are correct. Only
                // the trusted pin (or existing pairing's HMAC) can distinguish it.
                let proof = challenge.prove(&attacker_key, None).unwrap();
                crate::wire::response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    &serde_json::to_vec(&proof).unwrap(),
                )
                .await
                .unwrap();
                let mut extra = [0; 4096];
                match stream.read(&mut extra).await {
                    Ok(0) => {}
                    Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
                    result => panic!("client continued after a forged identity: {result:?}"),
                }
            };
            let client = async {
                let stream = smol::net::TcpStream::connect(address).await.unwrap();
                let result = open_websocket(
                    Box::new(stream),
                    &url::Url::parse(&host.origin).unwrap(),
                    &host,
                    &device,
                    true,
                )
                .await;
                assert!(matches!(result, Err(ConnectionFailure::Unreachable)));
            };
            futures_lite::future::race(futures_lite::future::zip(attacker, client), async {
                smol::Timer::after(Duration::from_secs(3)).await;
                panic!("forged endpoint exchange stalled");
            })
            .await;
        });
    }
}

#[test]
fn unauthenticated_http_identity_rejects_large_declarations_before_reading_the_body() {
    use futures_lite::io::{AsyncReadExt as _, AsyncWriteExt as _};

    for pin in [Some("12".repeat(32)), None] {
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Length: 4097\r\n\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Length: 16777216\r\n\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551615\r\n\r\n".to_owned(),
            format!("HTTP/1.1 200 OK\r\nX-Padding: {}", "x".repeat(4096)),
        ] {
            smol::block_on(async {
                let listener = smol::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let host = PairedHost {
                    host_id: "saved-workstation".into(),
                    name: "Saved workstation".into(),
                    origin: "http://192.168.1.161:47420".into(),
                    token: "A".repeat(43),
                    identity_key: pin.clone(),
                    candidates: Vec::new(),
                    last_connected_unix: None,
                };
                let device = DeviceIdentity {
                    id: "phone".into(),
                    name: "Phone".into(),
                    platform: None,
                };
                let attacker = async {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let request = crate::wire::read_request(&mut stream).await.unwrap();
                    assert_eq!(request.path, "/identity");
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    assert!(body.get("token").is_none());
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.flush().await.unwrap();
                    // Never send the declared payload. A late, post-allocation
                    // size check would wait here until the handshake timeout.
                    let mut extra = [0; 1];
                    match stream.read(&mut extra).await {
                        Ok(0) => {}
                        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
                        result => panic!("client did not close oversized preflight: {result:?}"),
                    }
                };
                let client = async {
                    let stream = smol::net::TcpStream::connect(address).await.unwrap();
                    let opened = open_websocket(
                        Box::new(stream),
                        &url::Url::parse(&host.origin).unwrap(),
                        &host,
                        &device,
                        true,
                    )
                    .await;
                    assert!(matches!(opened, Err(ConnectionFailure::Unreachable)));
                };
                futures_lite::future::race(futures_lite::future::zip(attacker, client), async {
                    smol::Timer::after(Duration::from_secs(2)).await;
                    panic!("oversized preflight was not rejected from its headers");
                })
                .await;
            });
        }
    }
}

// Pause after a real TCP write has emitted a WebSocket frame prefix. This
// makes cancellation observable without depending on OS socket buffer sizes.
struct PausedFrameWrite {
    stream: smol::net::TcpStream,
    prefix_written: bool,
    released: bool,
    blocked: Sender<()>,
    release: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
}

impl futures_lite::io::AsyncRead for PausedFrameWrite {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, bytes)
    }
}

impl futures_lite::io::AsyncWrite for PausedFrameWrite {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        if self.prefix_written && !self.released {
            if self.release.as_mut().poll(cx).is_pending() {
                return std::task::Poll::Pending;
            }
            self.released = true;
        }
        let bytes = if self.prefix_written {
            bytes
        } else {
            &bytes[..bytes.len().min(8)]
        };
        let result = std::pin::Pin::new(&mut self.stream).poll_write(cx, bytes);
        if matches!(result, std::task::Poll::Ready(Ok(count)) if count > 0) && !self.prefix_written
        {
            self.prefix_written = true;
            self.blocked.try_send(()).unwrap();
        }
        result
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_close(cx)
    }
}

#[test]
fn a_candidate_wake_during_a_partial_frame_keeps_the_socket_and_sends_each_message_once() {
    smol::block_on(async {
        let listener = smol::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, server) = futures_lite::future::zip(
            smol::net::TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept(),
        )
        .await;
        let (blocked, paused) = async_channel::unbounded();
        let (release, released) = async_channel::unbounded();
        let stream: crate::endpoint::Stream = Box::new(PausedFrameWrite {
            stream: client.unwrap(),
            prefix_written: false,
            released: false,
            blocked,
            release: Box::pin(async move {
                released.recv().await.unwrap();
            }),
        });
        let mut websocket =
            WebSocketStream::from_raw_socket(stream, tungstenite::protocol::Role::Client, None)
                .await;
        let mut peer = WebSocketStream::from_raw_socket(
            server.unwrap().0,
            tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let (to_host, outgoing) = tcode_client::outgoing::channel();
        let (incoming, _messages) = async_channel::unbounded();
        let (state, _states) = async_channel::unbounded();
        let mut subscriptions = HashMap::new();
        let mut buffered = VecDeque::new();
        let mut interfaces = InterfaceWatch::new(local_networks());
        let mut host = PairedHost {
            host_id: "workstation".into(),
            name: "Workstation".into(),
            origin: "http://192.168.31.42:47420".into(),
            token: "A".repeat(43),
            identity_key: Some("12".repeat(32)),
            candidates: Vec::new(),
            last_connected_unix: None,
        };
        let hint = "http://192.168.1.161:47420";
        let exercise = async {
            let first = r#"{"id":1,"payload":{"type":"command","content":{"type":"create_project","content":{"root":"/tmp/first"}}}}"#;
            let second = r#"{"id":2,"payload":{"type":"command","content":{"type":"create_project","content":{"root":"/tmp/second"}}}}"#;
            to_host.send(first.into()).await.unwrap();
            paused.recv().await.unwrap();
            to_host.wake(Wake::Candidates(vec![hint.into()]));
            while !outgoing.wake.is_empty() {
                futures_lite::future::yield_now().await;
            }
            release.send(()).await.unwrap();
            assert_eq!(
                peer.next().await.unwrap().unwrap(),
                Message::Text(first.into())
            );
            to_host.send(second.into()).await.unwrap();
            assert_eq!(
                peer.next().await.unwrap().unwrap(),
                Message::Text(second.into()),
                "a cancelled frame must never be replayed"
            );
            while to_host.queued() != 0 {
                futures_lite::future::yield_now().await;
            }
            to_host.wake(Wake::Reconnect);
        };
        let relay = async {
            let lost = relay_connected(
                &mut websocket,
                &outgoing,
                &incoming,
                &mut subscriptions,
                &mut buffered,
                &state,
                &mut host,
                &mut interfaces,
            )
            .await;
            assert!(
                matches!(lost.wake, Some(Wake::Reconnect)),
                "discovery interrupted a healthy send"
            );
        };
        futures_lite::future::race(futures_lite::future::zip(relay, exercise), async {
            smol::Timer::after(Duration::from_secs(3)).await;
            panic!("candidate wake abandoned a partially written frame");
        })
        .await;
        assert_eq!(host.candidates, [hint]);
        assert!(buffered.is_empty());
        assert_eq!(to_host.queued(), 0);
    });
}

#[test]
fn a_verified_reconnect_to_the_same_origin_persists_new_discovery_hints() {
    let machine = TestProfile::new();
    let client_profile = TestProfile::new();
    let (mux, _) = recovery_host();
    let server = crate::serve(mux, recovery_config(&machine)).unwrap();
    let origin = format!("http://{}", server.local_addr());
    let device = DeviceIdentity {
        id: "same-origin-phone".into(),
        name: "Phone".into(),
        platform: None,
    };
    let code = server.new_pairing_code();
    let mut host = pair(&origin, &code.code, &device).unwrap();
    let (socket, reported, key) =
        smol::block_on(attempt_origin(&origin, &host, &device, true)).unwrap();
    drop(socket);
    assert_eq!(host.identity_key, key);
    host.add_candidates(reported.iter().map(String::as_str));
    save_hosts(&client_profile.0, std::slice::from_ref(&host)).unwrap();
    let original = host.clone();
    let (started, first_attempt) = async_channel::unbounded();
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let establish = move |host: PairedHost, device: DeviceIdentity, _race: bool, _round: u32| {
        let first = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let started = started.clone();
        async move {
            if first {
                started.send(()).await.unwrap();
                std::future::pending::<()>().await;
            }
            let (websocket, reported, identity_key) =
                attempt_origin(&host.origin, &host, &device, true).await?;
            Ok(Established {
                websocket,
                origin: host.origin,
                reported,
                identity_key,
            })
        }
    };
    let (to_host, outgoing) = tcode_client::outgoing::channel();
    let (incoming, _messages) = async_channel::unbounded();
    let (state, states) = async_channel::unbounded();
    let data_dir = client_profile.0.clone();
    let loop_thread = std::thread::spawn(move || {
        smol::block_on(connection_loop(
            LiveHost::new(host),
            device,
            Some(data_dir),
            outgoing,
            incoming,
            state,
            establish,
        ))
    });
    let hint = "http://192.168.87.77:47420";
    smol::block_on(futures_lite::future::race(
        async {
            first_attempt.recv().await.unwrap();
            assert_eq!(
                load_hosts(&client_profile.0).unwrap().as_slice(),
                std::slice::from_ref(&original)
            );
            to_host.wake(Wake::Candidates(vec![hint.into()]));
            loop {
                match states.recv().await.unwrap() {
                    ConnectionState::Syncing => break,
                    ConnectionState::Offline { reason } => {
                        panic!("same-origin reconnect failed: {reason:?}")
                    }
                    _ => {}
                }
            }
        },
        async {
            smol::Timer::after(Duration::from_secs(3)).await;
            panic!("same-origin reconnect never authenticated");
        },
    ));
    let saved = load_hosts(&client_profile.0).unwrap().remove(0);
    assert_eq!(saved.origin, original.origin);
    assert_eq!(saved.identity_key, original.identity_key);
    assert_eq!(saved.token, original.token);
    assert!(
        saved.candidates.iter().any(|origin| origin == hint),
        "an unchanged winning origin lost a newly learned hint"
    );
    to_host.close();
    loop_thread.join().unwrap();
    server.shutdown();
}
