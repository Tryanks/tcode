use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_lite::io::AsyncWriteExt as _;
use futures_util::StreamExt as _;
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use serde_json::{Value, json};
use tcode_client::host::{ClientHost as _, PairRequest};
use tcode_remote::{HostMux, NativeClientHost, RemoteConfig, RemoteServer, serve};
use tungstenite::Message;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tcode-invite-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Host {
    server: Option<RemoteServer>,
    _data: TestDir,
    _commands: async_channel::Receiver<String>,
    _events: async_channel::Sender<String>,
}

impl Host {
    fn new() -> Self {
        let data = TestDir::new();
        let (to_host, commands) = async_channel::unbounded();
        let (events, from_host) = async_channel::unbounded();
        let server = serve(
            HostMux::new(to_host, from_host),
            RemoteConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                host_name: "Invited desktop".into(),
                data_dir: data.0.clone(),
                static_bundle: None,
                browser_password: false,
            },
        )
        .unwrap();
        Self {
            server: Some(server),
            _data: data,
            _commands: commands,
            _events: events,
        }
    }

    fn server(&self) -> &RemoteServer {
        self.server.as_ref().unwrap()
    }

    fn origin(&self) -> String {
        format!("http://{}", self.server().local_addr())
    }

    fn invite(&self) -> PairRequest {
        let code = self.server().new_pairing_code();
        PairRequest {
            origin: self.origin(),
            code: code.code,
            host_id: Some(code.host_id),
            identity_key: Some(code.identity_key),
            candidates: Vec::new(),
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.server.take().unwrap().shutdown();
    }
}

/// A peer that copies the requested host id and nonce, but signs with its own
/// key. Record the entire exchange to detect disclosure before authentication.
struct Impostor {
    origin: String,
    accepted: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<Value>>>,
    settled: async_channel::Receiver<()>,
    _task: smol::Task<()>,
}

#[derive(Clone, Copy)]
enum Attack {
    WrongKey,
    OversizedFrame,
}

impl Impostor {
    async fn new(attack: Attack) -> Self {
        let listener = smol::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let capture = received.clone();
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = accepted.clone();
        let (settled_tx, settled) = async_channel::unbounded();
        let task = smol::spawn(async move {
            let mut connections = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let capture = capture.clone();
                let settled = settled_tx.clone();
                connections.push(smol::spawn(async move {
                    if let Ok(mut socket) = async_tungstenite::accept_async(stream).await {
                        while let Some(Ok(Message::Text(text))) = socket.next().await {
                            let message: Value = serde_json::from_str(&text).unwrap();
                            capture.lock().unwrap().push(message.clone());
                            if message["type"] == "identify" {
                                if matches!(attack, Attack::OversizedFrame) {
                                    // Only the frame header, declaring 16 MiB.
                                    // Rejecting before payload reads prevents
                                    // tungstenite reserving this claimed size.
                                    let mut header = vec![0x81, 127];
                                    header.extend_from_slice(&(16_u64 * 1024 * 1024).to_be_bytes());
                                    if socket.get_mut().write_all(&header).await.is_err() {
                                        break;
                                    }
                                    continue;
                                }
                                let key = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
                                let nonce = message["nonce"].as_str().unwrap();
                                let host_id = message["host_id"].as_str().unwrap();
                                let mut signed = b"tcode-host-identity-v1\0".to_vec();
                                signed.extend_from_slice(host_id.as_bytes());
                                signed.push(0);
                                signed.extend((0..nonce.len()).step_by(2).map(|index| {
                                    u8::from_str_radix(&nonce[index..index + 2], 16).unwrap()
                                }));
                                signed.extend_from_slice(key.public_key().as_ref());
                                let hex = |bytes: &[u8]| {
                                    bytes
                                        .iter()
                                        .map(|byte| format!("{byte:02x}"))
                                        .collect::<String>()
                                };
                                let forged = json!({
                                    "type": "identity", "host_id": host_id, "nonce": nonce,
                                    "identity_key": hex(key.public_key().as_ref()),
                                    "signature": hex(key.sign(&signed).as_ref()),
                                });
                                if socket
                                    .send(Message::Text(forged.to_string().into()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = settled.send(()).await;
                }));
            }
        });
        Self {
            origin,
            accepted,
            received,
            settled,
            _task: task,
        }
    }

    async fn assert_attempt_disclosed_no_code(&self) {
        futures_lite::future::race(async { self.settled.recv().await.unwrap() }, async {
            smol::Timer::after(Duration::from_secs(30)).await;
            panic!("impostor connection remained open after pairing finished");
        })
        .await;
        let received = self.received.lock().unwrap();
        assert!(
            !received.is_empty(),
            "the wrong primary must actually be tried"
        );
        assert!(received.iter().all(|message| message["type"] == "identify"));
        assert!(
            received
                .iter()
                .all(|message| message.get("code").is_none() && message.get("token").is_none())
        );
    }
}

#[test]
fn native_qr_pairing_uses_an_authenticated_alternate_and_consumes_one_pair_code() {
    let desktop = Host::new();
    let data = TestDir::new();
    let phone = NativeClientHost::new(data.0.clone(), "Phone");
    smol::block_on(async {
        let impostor = Impostor::new(Attack::WrongKey).await;
        let mut request = desktop.invite();
        let expected_key = request.identity_key.clone();
        request.identity_key = request.identity_key.map(|key| key.to_ascii_uppercase());
        request.origin = impostor.origin.clone();
        request.candidates = vec![desktop.origin(), desktop.origin()];
        let paired = phone.pair(request.clone()).await.unwrap();
        assert_eq!(paired.origin, desktop.origin());
        assert_eq!(paired.host_id, request.host_id.clone().unwrap());
        assert_eq!(paired.identity_key, expected_key);
        assert_eq!(paired.token.len(), 43);
        assert!(paired.candidates.contains(&impostor.origin));
        assert_eq!(desktop.server().devices().len(), 1);
        impostor.assert_attempt_disclosed_no_code().await;

        // An explicit second attempt with the consumed code fails and cannot
        // create another paired-device record.
        assert!(phone.pair(request.clone()).await.is_err());
        assert_eq!(desktop.server().devices().len(), 1);
        impostor.assert_attempt_disclosed_no_code().await;
    });
}

#[test]
fn a_copied_host_id_with_a_different_signing_key_never_receives_the_pair_code() {
    let desktop = Host::new();
    let data = TestDir::new();
    let phone = NativeClientHost::new(data.0.clone(), "Phone");
    smol::block_on(async {
        let impostor = Impostor::new(Attack::WrongKey).await;
        let correct = desktop.invite();
        let mut forged = correct.clone();
        forged.origin = impostor.origin.clone();
        assert!(phone.pair(forged).await.is_err());
        impostor.assert_attempt_disclosed_no_code().await;
        assert!(desktop.server().devices().is_empty());

        let paired = phone.pair(correct).await.unwrap();
        assert_eq!(paired.origin, desktop.origin());
        assert_eq!(desktop.server().devices().len(), 1);
    });
}

#[test]
fn an_older_qr_without_a_pin_uses_only_its_original_address() {
    let desktop = Host::new();
    let data = TestDir::new();
    let phone = NativeClientHost::new(data.0.clone(), "Phone");
    smol::block_on(async {
        let unused = Impostor::new(Attack::WrongKey).await;
        let mut request = desktop.invite();
        request.identity_key = None;
        request.candidates = vec![unused.origin.clone()];
        let paired = phone.pair(request).await.unwrap();
        assert_eq!(paired.origin, desktop.origin());
        assert_eq!(desktop.server().devices().len(), 1);
        assert_eq!(unused.accepted.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn a_qr_candidate_cannot_request_a_large_frame_allocation_before_identity_verification() {
    let desktop = Host::new();
    let data = TestDir::new();
    let phone = NativeClientHost::new(data.0.clone(), "Phone");
    smol::block_on(async {
        let impostor = Impostor::new(Attack::OversizedFrame).await;
        let mut request = desktop.invite();
        request.origin = impostor.origin.clone();
        futures_lite::future::race(
            async {
                assert!(phone.pair(request).await.is_err());
                impostor.assert_attempt_disclosed_no_code().await;
            },
            async {
                smol::Timer::after(Duration::from_secs(30)).await;
                panic!("pairing waited for the oversized frame body");
            },
        )
        .await;
        assert!(desktop.server().devices().is_empty());
    });
}
