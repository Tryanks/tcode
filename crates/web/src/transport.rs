//! A single browser task owns the socket and replay queue. Every browser
//! callback only enqueues an event; dropping a connection removes callbacks,
//! event listeners and timers so stale sockets cannot affect a newer attempt.
use std::collections::{BTreeMap, VecDeque};

use async_channel::{Receiver, Sender};
use futures_lite::future::race;
use tcode_mobile::host::{ConnectionState, Transport};
use wasm_bindgen::{JsCast as _, prelude::*};

use crate::host::window;

enum Event {
    Open,
    Text(String),
    Lost,
    Wake,
    Timeout,
}

struct Listener {
    target: web_sys::EventTarget,
    name: &'static str,
    callback: Closure<dyn FnMut(web_sys::Event)>,
}

impl Listener {
    fn new(
        target: &web_sys::EventTarget,
        name: &'static str,
        f: impl FnMut(web_sys::Event) + 'static,
    ) -> Self {
        let callback = Closure::wrap(Box::new(f) as Box<dyn FnMut(_)>);
        target
            .add_event_listener_with_callback(name, callback.as_ref().unchecked_ref())
            .expect("browser event listener");
        Self {
            target: target.clone(),
            name,
            callback,
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self
            .target
            .remove_event_listener_with_callback(self.name, self.callback.as_ref().unchecked_ref());
    }
}

struct Timer {
    id: i32,
    _callback: Closure<dyn FnMut()>,
}

impl Timer {
    fn new(milliseconds: i32, tx: Sender<Event>) -> Self {
        let callback = Closure::wrap(Box::new(move || {
            let _ = tx.try_send(Event::Timeout);
        }) as Box<dyn FnMut()>);
        let id = window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                callback.as_ref().unchecked_ref(),
                milliseconds,
            )
            .expect("browser timeout");
        Self {
            id,
            _callback: callback,
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        window().clear_timeout_with_handle(self.id);
    }
}

struct Socket {
    ws: web_sys::WebSocket,
    _listeners: Vec<Listener>,
}

impl Socket {
    fn new(tx: &Sender<Event>) -> Result<Self, JsValue> {
        let location = window().location();
        let scheme = if location.protocol()? == "https:" {
            "wss"
        } else {
            "ws"
        };
        let ws = web_sys::WebSocket::new(&format!("{scheme}://{}/ws", location.host()?))?;
        let mut listeners = Vec::new();
        for (name, kind) in [("open", 0), ("message", 1), ("close", 2), ("error", 2)] {
            let tx = tx.clone();
            listeners.push(Listener::new(ws.as_ref(), name, move |event| {
                let event = match kind {
                    0 => Event::Open,
                    1 => match event
                        .dyn_into::<web_sys::MessageEvent>()
                        .ok()
                        .and_then(|event| event.data().as_string())
                    {
                        Some(text) => Event::Text(text),
                        None => Event::Lost,
                    },
                    _ => Event::Lost,
                };
                let _ = tx.try_send(event);
            }));
        }
        Ok(Self {
            ws,
            _listeners: listeners,
        })
    }

    fn send(&self, line: &str) -> Result<(), JsValue> {
        if self.ws.ready_state() != web_sys::WebSocket::OPEN {
            return Err(JsValue::from_str("socket is not open"));
        }
        self.ws.send_with_str(line.trim_end())
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self._listeners.clear();
        let _ = self.ws.close();
    }
}

pub fn connect(token: String, device_name: String) -> Transport {
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let (state_tx, state) = async_channel::unbounded();
    wasm_bindgen_futures::spawn_local(async move {
        // Receiver-only closure must also wake an idle connection/backoff.
        race(
            connection_loop(token, device_name, &outgoing, &incoming, &state_tx),
            async {
                race(incoming.closed(), state_tx.closed()).await;
            },
        )
        .await;
        let _ = state_tx.try_send(ConnectionState::Offline);
        incoming.close();
        outgoing.close();
    });
    Transport {
        to_host,
        from_host,
        state,
    }
}

enum Input {
    Line(Result<String, async_channel::RecvError>),
    Event(Event),
}

async fn next(outgoing: &Receiver<String>, events: &Receiver<Event>) -> Input {
    race(async { Input::Line(outgoing.recv().await) }, async {
        Input::Event(events.recv().await.unwrap_or(Event::Lost))
    })
    .await
}

fn subscription_key(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    let payload = value.get("payload")?;
    if !matches!(payload.get("type")?.as_str()?, "subscribe" | "unsubscribe") {
        return None;
    }
    serde_json::to_string(payload.get("content")?.get("topic")?).ok()
}

async fn connection_loop(
    token: String,
    device_name: String,
    outgoing: &Receiver<String>,
    incoming: &Sender<String>,
    state: &Sender<ConnectionState>,
) {
    let mut subscriptions = BTreeMap::<String, String>::new();
    let mut buffered = VecDeque::<String>::new();
    let mut attempt = 1_u32;
    let mut delay = 0;
    loop {
        if outgoing.is_closed() {
            return;
        }
        let _ = state.try_send(ConnectionState::Reconnecting { attempt });
        // Each attempt has its own event queue, including foreground wakeups.
        let (tx, events) = async_channel::unbounded();
        let wake_tx = tx.clone();
        let _online = Listener::new(window().as_ref(), "online", move |_| {
            let _ = wake_tx.try_send(Event::Wake);
        });
        let wake_tx = tx.clone();
        let document = window().document().expect("browser document");
        let _visible = Listener::new(document.as_ref(), "visibilitychange", move |_| {
            if !window().document().unwrap().hidden() {
                let _ = wake_tx.try_send(Event::Wake);
            }
        });
        if delay > 0 {
            let _timer = Timer::new(delay, tx.clone());
            loop {
                match next(outgoing, &events).await {
                    Input::Line(Ok(line)) => remember(line, &mut subscriptions, &mut buffered),
                    Input::Line(Err(_)) => return,
                    Input::Event(Event::Timeout | Event::Wake) => break,
                    _ => {}
                }
            }
        }
        // Discard any backoff timer/wakeup already queued before opening.
        while events.try_recv().is_ok() {}
        let mut immediate = false;
        if let Ok(socket) = Socket::new(&tx) {
            let mut handshake_timer = Some(Timer::new(5000, tx.clone()));
            let mut ready = false;
            loop {
                match next(outgoing, &events).await {
                    Input::Line(Err(_)) => return,
                    Input::Line(Ok(line)) => {
                        if let Some(key) = subscription_key(&line) {
                            subscriptions.insert(key, line.clone());
                        }
                        if !ready || socket.send(&line).is_err() {
                            if subscription_key(&line).is_none() {
                                buffered.push_back(line);
                            }
                            if ready {
                                break;
                            }
                        }
                    }
                    Input::Event(Event::Open) => {
                        let hello = serde_json::json!({"type":"hello", "protocol_version":tcode_protocol::PROTOCOL_VERSION, "token":token, "device_name":device_name});
                        if socket.send(&hello.to_string()).is_err() {
                            break;
                        }
                    }
                    Input::Event(Event::Text(line)) if !ready => {
                        let hello: serde_json::Value =
                            serde_json::from_str(&line).unwrap_or_default();
                        if hello["type"].as_str() != Some("hello_ok")
                            || hello["protocol_version"].as_u64()
                                != Some(u64::from(tcode_protocol::PROTOCOL_VERSION))
                        {
                            break;
                        }
                        handshake_timer.take();
                        if subscriptions
                            .values()
                            .any(|line| socket.send(line).is_err())
                        {
                            break;
                        }
                        let mut failed = false;
                        while let Some(line) = buffered.front() {
                            if socket.send(line).is_err() {
                                failed = true;
                                break;
                            }
                            buffered.pop_front();
                        }
                        if failed {
                            break;
                        }
                        ready = true;
                        attempt = 0;
                        let _ = state.try_send(ConnectionState::Connected);
                    }
                    Input::Event(Event::Text(line)) => {
                        if incoming.try_send(format!("{}\n", line.trim_end())).is_err() {
                            return;
                        }
                    }
                    Input::Event(Event::Wake) => {
                        immediate = true;
                        break;
                    }
                    Input::Event(Event::Timeout) if ready => {}
                    Input::Event(Event::Lost | Event::Timeout) => break,
                }
            }
        }
        delay = if immediate {
            0
        } else {
            (1000_i32 << attempt.saturating_sub(1).min(5)).min(30_000)
        };
        attempt = attempt.saturating_add(1).max(1);
    }
}

fn remember(
    line: String,
    subscriptions: &mut BTreeMap<String, String>,
    buffered: &mut VecDeque<String>,
) {
    if let Some(key) = subscription_key(&line) {
        subscriptions.insert(key, line);
    } else {
        buffered.push_back(line);
    }
}
