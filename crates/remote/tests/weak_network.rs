//! Real TCP/WebSocket loss with production runtime, mux, transport and link.
use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tcode_client::{ConnectionState, HostLink, host::ClientHost};
use tcode_protocol::{Command, Query, Subscription, Topic};
use tcode_remote::{HostMux, NativeClientHost, RemoteConfig, client, serve};

struct Relay {
    addr: SocketAddr,
    paused: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Relay {
    fn new(target: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let paused = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, e, s) = (paused.clone(), epoch.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut workers = Vec::new();
            while !s.load(Ordering::SeqCst) {
                if let Ok((client, _)) = listener.accept() {
                    let server = TcpStream::connect(target).unwrap();
                    let generation = e.load(Ordering::SeqCst);
                    for (mut source, mut dest, downstream) in [
                        (
                            client.try_clone().unwrap(),
                            server.try_clone().unwrap(),
                            false,
                        ),
                        (server, client, true),
                    ] {
                        let (p, e, s) = (p.clone(), e.clone(), s.clone());
                        workers.push(std::thread::spawn(move || {
                            source
                                .set_read_timeout(Some(Duration::from_millis(20)))
                                .unwrap();
                            dest.set_write_timeout(Some(Duration::from_millis(100)))
                                .unwrap();
                            let mut bytes = [0; 16384];
                            while !s.load(Ordering::SeqCst)
                                && e.load(Ordering::SeqCst) == generation
                            {
                                if downstream && p.load(Ordering::SeqCst) {
                                    std::thread::sleep(Duration::from_millis(5));
                                    continue;
                                }
                                match source.read(&mut bytes) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        while downstream
                                            && p.load(Ordering::SeqCst)
                                            && !s.load(Ordering::SeqCst)
                                            && e.load(Ordering::SeqCst) == generation
                                        {
                                            std::thread::sleep(Duration::from_millis(5));
                                        }
                                        if s.load(Ordering::SeqCst)
                                            || e.load(Ordering::SeqCst) != generation
                                        {
                                            break;
                                        }
                                        if dest.write_all(&bytes[..n]).is_err() {
                                            break;
                                        }
                                    }
                                    Err(error)
                                        if matches!(
                                            error.kind(),
                                            std::io::ErrorKind::WouldBlock
                                                | std::io::ErrorKind::TimedOut
                                        ) => {}
                                    Err(_) => break,
                                }
                            }
                            let _ = source.shutdown(Shutdown::Both);
                            let _ = dest.shutdown(Shutdown::Both);
                        }));
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            addr,
            paused,
            epoch,
            stop,
            thread: Some(thread),
        }
    }
    fn drop_sockets(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn link(host: &tcode_client::pairing::PairedHost, adapter: &NativeClientHost) -> HostLink {
    let transport = adapter.connect(host);
    let link = HostLink::new(transport.to_host, transport.from_host);
    link.set_connection_state(ConnectionState::Reconnecting {
        attempt: 1,
        reason: None,
    });
    link.restore_outbox(adapter.outbox_storage(&host.host_id).unwrap())
        .unwrap();
    let pump = link.clone();
    smol::spawn(async move { pump.pump().await }).detach();
    let states = link.clone();
    smol::spawn(async move {
        while let Ok(state) = transport.state.recv().await {
            states.set_connection_state(state);
        }
    })
    .detach();
    link.subscribe(Subscription {
        topic: Topic::Settings,
        after: None,
    })
    .unwrap();
    until(|| link.connection_state() == ConnectionState::Connected);
    link
}
#[test]
fn relay_disconnect_fails_read_and_recreation_replays_write_without_execution_twice() {
    let root = std::env::temp_dir().join(format!("tcode-weak-{}", uuid::Uuid::new_v4()));
    let host = tcode_runtime::pipe::spawn_host(
        tcode_services::store::SessionStore::open_at(root.join("host")).unwrap(),
        Default::default(),
    )
    .unwrap();
    let (observed_tx, observed_rx) = async_channel::unbounded::<String>();
    let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let trace = delivered.clone();
    let to_runtime = host.to_host.clone();
    let observer = std::thread::spawn(move || {
        while let Ok(line) = observed_rx.recv_blocking() {
            if let Ok(message) = tcode_protocol::decode_client_line(&line) {
                trace.lock().unwrap().push(message);
            }
            if to_runtime.send_blocking(line).is_err() {
                break;
            }
        }
    });
    let mux = HostMux::new(observed_tx, host.from_host.clone());
    let server = serve(
        mux.clone(),
        RemoteConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            host_name: "test".into(),
            data_dir: root.join("remote"),
            static_bundle: None,
            browser_password: false,
        },
    )
    .unwrap();
    let relay = Relay::new(server.local_addr());
    let paired = client::pair(
        &format!("http://{}", relay.addr),
        &server.new_pairing_code().code,
        "test-device",
    )
    .unwrap();
    let adapter = NativeClientHost::new(root.join("client"), "test-device");
    let first = link(&paired, &adapter);
    relay.paused.store(true, Ordering::SeqCst);
    let read_link = first.clone();
    let (tx, rx) = async_channel::bounded(1);
    smol::spawn(async move {
        tx.send(read_link.query(Query::Ping).await).await.unwrap();
    })
    .detach();
    let sort = || {
        smol::block_on(host.update_state_for_test(|state, _| state.settings.project_sort)).unwrap()
    };
    let before = sort();
    let write_link = first.clone();
    let (ack_tx, ack_rx) = async_channel::bounded(1);
    smol::spawn(async move {
        ack_tx
            .send(write_link.command(Command::CycleProjectSort).await)
            .await
            .unwrap();
    })
    .detach();
    until(|| {
        delivered.lock().unwrap().iter().any(|message| {
            matches!(
                message.payload,
                tcode_protocol::ClientPayload::Query(Query::Ping)
            )
        })
    });
    until(|| sort() != before);
    let executed_once = sort();
    let key = first.pending_commands()[0].0.clone();
    relay.drop_sockets();
    until(|| {
        matches!(
            first.connection_state(),
            ConnectionState::Reconnecting { .. }
        )
    });
    let start = Instant::now();
    let result = smol::block_on(async {
        futures_lite::future::race(rx.recv(), async {
            smol::Timer::after(Duration::from_millis(100)).await;
            panic!("query waiter hung");
        })
        .await
    })
    .unwrap();
    assert_eq!(result.unwrap_err().code, "disconnected");
    assert!(start.elapsed() < Duration::from_millis(100));
    assert_eq!(first.pending_commands()[0].0, key);
    assert!(
        ack_rx.try_recv().is_err(),
        "a disconnected write must remain pending"
    );
    relay.paused.store(false, Ordering::SeqCst);
    until(|| !ack_rx.is_empty());
    assert_eq!(
        ack_rx.recv_blocking().unwrap().unwrap(),
        tcode_protocol::CommandResponse::Unit
    );
    assert_eq!(
        sort(),
        executed_once,
        "cached Ack must prevent cycling the sort twice"
    );

    relay.paused.store(true, Ordering::SeqCst);
    relay.drop_sockets();
    until(|| {
        matches!(
            first.connection_state(),
            ConnectionState::Reconnecting { .. }
        )
    });
    first.dispatch(Command::CycleProjectSort).unwrap();
    let recreated_key = first.pending_commands()[0].0.clone();
    first.close();
    relay.paused.store(false, Ordering::SeqCst);
    let second = link(&paired, &adapter);
    until(|| second.pending_commands().is_empty());
    assert_ne!(
        sort(),
        executed_once,
        "the persisted second action must execute after recreation"
    );
    let redeliveries: Vec<_> = delivered
        .lock()
        .unwrap()
        .iter()
        .filter(|message| {
            matches!(
                message.payload,
                tcode_protocol::ClientPayload::Command(Command::CycleProjectSort)
            )
        })
        .cloned()
        .collect();
    assert_eq!(redeliveries.len(), 3);
    assert_eq!(redeliveries[0].key, redeliveries[1].key);
    assert!(redeliveries[0].key.as_ref().unwrap().ends_with(&key));
    assert!(
        redeliveries[2]
            .key
            .as_ref()
            .unwrap()
            .ends_with(&recreated_key)
    );
    assert_ne!(redeliveries[1].key, redeliveries[2].key);
    assert!(
        adapter
            .outbox_storage(&paired.host_id)
            .unwrap()
            .load()
            .unwrap()
            .is_empty()
    );
    second.close();
    drop(relay);
    server.shutdown();
    let local = mux.attach();
    let local = HostLink::new(local.to_host, local.from_host);
    let pump = local.clone();
    smol::spawn(async move { pump.pump().await }).detach();
    local.shutdown_blocking().unwrap();
    host.to_host.close();
    smol::block_on(host.stopped.recv()).unwrap();
    observer.join().unwrap();
    let _ = std::fs::remove_dir_all(root);
}
