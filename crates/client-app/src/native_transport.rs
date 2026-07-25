//! A native WebSocket transport, shared by the Android and iOS shells.
//!
//! Both are ordinary native targets with threads and sockets, so unlike the
//! browser they can use the same tokio-tungstenite client the desktop probe
//! uses. Keeping it here rather than in two shells means the framing and
//! disconnect behaviour cannot drift between the two phones.

use std::sync::Arc;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::{Inbox, Transport, TransportEvent, TransportFactory};

/// Owns the tokio runtime every socket runs on.
///
/// One runtime for the process, built once. A phone has few cores, and giving
/// each reconnect its own runtime would spawn a fresh thread pool every time a
/// train enters a tunnel.
pub struct NativeTransportFactory {
    runtime: Arc<Runtime>,
}

impl NativeTransportFactory {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            runtime: Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()?,
            ),
        })
    }
}

/// A connected socket.
///
/// Sends are queued rather than awaited: the caller is GPUI's render thread and
/// must never block on the network, however briefly.
pub struct NativeTransport {
    outgoing: mpsc::UnboundedSender<String>,
}

impl Transport for NativeTransport {
    fn send(&self, text: &str) -> Result<(), String> {
        self.outgoing
            .send(text.to_owned())
            .map_err(|_| "the connection is closed".to_owned())
    }
}

fn deliver(inbox: &Inbox, event: TransportEvent) {
    inbox
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_back(event);
}

impl TransportFactory for NativeTransportFactory {
    fn connect(&self, url: &str, hello: &str, inbox: Inbox) -> Result<Box<dyn Transport>, String> {
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<String>();
        let url = url.to_owned();
        let hello = hello.to_owned();

        self.runtime.spawn(async move {
            let (socket, _) = match tokio_tungstenite::connect_async(&url).await {
                Ok(connected) => connected,
                Err(error) => {
                    // Reported as a close rather than an error so the client's
                    // reconnect path handles it: a host that is not up yet is
                    // the same situation as one that went away.
                    deliver(&inbox, TransportEvent::Closed(error.to_string()));
                    return;
                }
            };
            let (mut sink, mut stream) = socket.split();

            if sink.send(Message::Text(hello.into())).await.is_err() {
                deliver(&inbox, TransportEvent::Error);
                return;
            }
            deliver(&inbox, TransportEvent::Open);

            loop {
                tokio::select! {
                    outgoing = outgoing_rx.recv() => {
                        let Some(text) = outgoing else {
                            // The client dropped its handle: close cleanly so
                            // the host frees the subscription instead of
                            // waiting for a timeout.
                            let _ = sink.close().await;
                            break;
                        };
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            deliver(&inbox, TransportEvent::Error);
                            break;
                        }
                    }
                    incoming = stream.next() => {
                        match incoming {
                            Some(Ok(Message::Text(text))) => {
                                deliver(&inbox, TransportEvent::Text(text.to_string()));
                            }
                            Some(Ok(Message::Close(_))) | None => {
                                deliver(&inbox, TransportEvent::Closed(String::new()));
                                break;
                            }
                            // Ping/pong are handled by tungstenite; binary
                            // frames are not part of this protocol.
                            Some(Ok(_)) => {}
                            Some(Err(error)) => {
                                deliver(&inbox, TransportEvent::Closed(error.to_string()));
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(Box::new(NativeTransport {
            outgoing: outgoing_tx,
        }))
    }
}
