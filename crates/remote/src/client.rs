use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use async_tungstenite::WebSocketStream;
use futures_util::{FutureExt as _, StreamExt as _};
use serde::Deserialize;
use smol::Async;
use tungstenite::Message;

pub use tcode_client::ConnectionState;
pub use tcode_client::pairing::{
    PairInvite, PairedHost, is_pairing_code, pair_url, parse_pair_url,
};

pub struct RemoteClient {
    pub to_host: Sender<String>,
    pub from_host: Receiver<String>,
    pub state: Receiver<ConnectionState>,
}

#[derive(Deserialize)]
struct PairResponse {
    host_id: String,
    host_name: String,
    token: String,
}

pub fn pair(addr: &str, port: u16, code: &str, device_name: &str) -> Result<PairedHost, String> {
    let authority = authority(addr, port);
    let url = format!("http://{authority}/pair");
    let body = serde_json::json!({ "code": code, "device_name": device_name }).to_string();
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body)
        .map_err(|error| error.to_string())?;
    let response: PairResponse =
        serde_json::from_reader(response.into_reader()).map_err(|error| error.to_string())?;
    Ok(PairedHost {
        host_id: response.host_id,
        name: response.host_name,
        addrs: vec![addr.to_owned()],
        port,
        token: response.token,
        last_connected_unix: None,
    })
}

pub fn load_hosts(data_dir: &Path) -> io::Result<Vec<PairedHost>> {
    match fs::read(data_dir.join("hosts.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub fn save_hosts(data_dir: &Path, hosts: &[PairedHost]) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let bytes = serde_json::to_vec_pretty(hosts).map_err(io::Error::other)?;
    let temporary = data_dir.join("hosts.json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, data_dir.join("hosts.json"))
}

pub fn connect(host: PairedHost, device_name: String) -> RemoteClient {
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let (state_tx, state) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("tcode-remote-client".into())
        .spawn(move || {
            smol::block_on(connection_loop(
                host,
                device_name,
                outgoing,
                incoming,
                state_tx,
            ));
        })
        .expect("failed to spawn remote client thread");
    RemoteClient {
        to_host,
        from_host,
        state,
    }
}

async fn connection_loop(
    host: PairedHost,
    device_name: String,
    outgoing: Receiver<String>,
    incoming: Sender<String>,
    state: Sender<ConnectionState>,
) {
    let mut buffered = VecDeque::<String>::new();
    let mut subscriptions = HashMap::<String, String>::new();
    let mut attempt = 0_u32;
    while !outgoing.is_closed() && !incoming.is_closed() {
        attempt = attempt.saturating_add(1);
        let _ = state.send(ConnectionState::Reconnecting { attempt }).await;
        let mut connected = None;
        for address in &host.addrs {
            match open_websocket(address, host.port, &host.token, &device_name).await {
                Ok(websocket) => {
                    connected = Some(websocket);
                    break;
                }
                Err(error) => log::debug!("remote connection attempt failed: {error}"),
            }
        }
        if let Some(mut websocket) = connected {
            let mut ready = true;
            for line in subscriptions.values() {
                if websocket
                    .send(Message::Text(line.trim_end().to_owned().into()))
                    .await
                    .is_err()
                {
                    ready = false;
                    break;
                }
            }
            if ready {
                while let Some(line) = buffered.pop_front() {
                    if subscription_key(&line).is_some() {
                        continue;
                    }
                    if websocket
                        .send(Message::Text(line.trim_end().to_owned().into()))
                        .await
                        .is_err()
                    {
                        buffered.push_front(line);
                        ready = false;
                        break;
                    }
                }
            }
            if ready {
                attempt = 0;
                let _ = state.send(ConnectionState::Connected).await;
                relay_connected(
                    &mut websocket,
                    &outgoing,
                    &incoming,
                    &mut subscriptions,
                    &mut buffered,
                )
                .await;
            }
        }
        if outgoing.is_closed() || incoming.is_closed() {
            break;
        }
        let backoff_attempt = attempt.clamp(1, 6);
        let seconds = (1_u64 << (backoff_attempt - 1)).min(30);
        buffer_during_backoff(
            &outgoing,
            &mut buffered,
            &mut subscriptions,
            Duration::from_secs(seconds),
        )
        .await;
    }
    let _ = state.send(ConnectionState::Offline).await;
    incoming.close();
}

async fn open_websocket(
    address: &str,
    port: u16,
    token: &str,
    device_name: &str,
) -> Result<WebSocketStream<Async<TcpStream>>, String> {
    let socket = socket_addr(address, port)?;
    let stream = futures_lite::future::race(
        async {
            Async::<TcpStream>::connect(socket)
                .await
                .map_err(|error| error.to_string())
        },
        async {
            smol::Timer::after(Duration::from_secs(5)).await;
            Err("connection timed out".to_owned())
        },
    )
    .await?;
    let url = format!("ws://{}/ws", authority(address, port));
    let (mut websocket, _) = async_tungstenite::client_async(url, stream)
        .await
        .map_err(|error| error.to_string())?;
    let hello = serde_json::json!({
        "type": "hello",
        "protocol_version": 1,
        "token": token,
        "device_name": device_name,
    });
    websocket
        .send(Message::Text(hello.to_string().into()))
        .await
        .map_err(|error| error.to_string())?;
    match websocket.next().await {
        Some(Ok(Message::Text(text))) => {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("hello_ok") {
                Ok(websocket)
            } else {
                Err("host rejected remote hello".into())
            }
        }
        Some(Ok(_)) => Err("host sent a non-text hello response".into()),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("host closed during hello".into()),
    }
}

async fn relay_connected(
    websocket: &mut WebSocketStream<Async<TcpStream>>,
    outgoing: &Receiver<String>,
    incoming: &Sender<String>,
    subscriptions: &mut HashMap<String, String>,
    buffered: &mut VecDeque<String>,
) {
    loop {
        enum Input {
            Outgoing(Result<String, async_channel::RecvError>),
            WebSocket(Option<Result<Message, tungstenite::Error>>),
        }
        let input = {
            let outbound = outgoing.recv().fuse();
            let websocket_input = websocket.next().fuse();
            futures_util::pin_mut!(outbound, websocket_input);
            futures_util::select! {
                line = outbound => Input::Outgoing(line),
                message = websocket_input => Input::WebSocket(message),
            }
        };
        match input {
            Input::Outgoing(Ok(line)) => {
                remember_subscription(&line, subscriptions);
                if websocket
                    .send(Message::Text(line.trim_end().to_owned().into()))
                    .await
                    .is_err()
                {
                    buffered.push_back(line);
                    return;
                }
            }
            Input::Outgoing(Err(_)) => return,
            Input::WebSocket(Some(Ok(Message::Text(line)))) => {
                let mut line = line.to_string();
                if !line.ends_with('\n') {
                    line.push('\n');
                }
                if incoming.send(line).await.is_err() {
                    return;
                }
            }
            Input::WebSocket(Some(Ok(Message::Ping(payload)))) => {
                if websocket.send(Message::Pong(payload)).await.is_err() {
                    return;
                }
            }
            Input::WebSocket(Some(Ok(Message::Close(_))) | Some(Err(_)) | None) => return,
            Input::WebSocket(Some(Ok(_))) => {}
        }
    }
}

async fn buffer_during_backoff(
    outgoing: &Receiver<String>,
    buffered: &mut VecDeque<String>,
    subscriptions: &mut HashMap<String, String>,
    duration: Duration,
) {
    let deadline = futures_util::FutureExt::fuse(smol::Timer::after(duration));
    futures_util::pin_mut!(deadline);
    loop {
        enum Input {
            Line(Result<String, async_channel::RecvError>),
            Done,
        }
        let input = {
            let line = outgoing.recv().fuse();
            futures_util::pin_mut!(line);
            futures_util::select! {
                line = line => Input::Line(line),
                _ = deadline => Input::Done,
            }
        };
        match input {
            Input::Line(Ok(line)) => {
                remember_subscription(&line, subscriptions);
                buffered.push_back(line);
            }
            Input::Line(Err(_)) | Input::Done => return,
        }
    }
}

fn remember_subscription(line: &str, subscriptions: &mut HashMap<String, String>) {
    if let Some(key) = subscription_key(line) {
        subscriptions.insert(key, line.to_owned());
    }
}

fn subscription_key(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    let payload = value.get("payload")?;
    if payload.get("type")?.as_str()? != "subscribe" {
        return None;
    }
    serde_json::to_string(payload.get("content")?.get("topic")?).ok()
}

fn authority(address: &str, port: u16) -> String {
    if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]:{port}")
    } else {
        format!("{address}:{port}")
    }
}

fn socket_addr(address: &str, port: u16) -> Result<SocketAddr, String> {
    let ip: IpAddr = address
        .trim_matches(|character| character == '[' || character == ']')
        .parse()
        .map_err(|error| format!("invalid host address: {error}"))?;
    Ok(SocketAddr::new(ip, port))
}
