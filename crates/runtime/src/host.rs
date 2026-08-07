//! Dedicated-thread host execution seam.
//!
//! `AppState` is a plain `Send` value owned by the host loop. UI clients can
//! reach it only through typed protocol messages. Background completions use
//! the same mailbox via [`HostCx::enqueue`].

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, Topic};

use crate::app::AppState;
use crate::event::HostEvent;

pub(crate) type HostFn = Box<dyn FnOnce(&mut AppState, &mut HostCx) + Send + 'static>;

/// The single mailbox consumed by the thread that owns [`AppState`].
pub(crate) enum HostMsg {
    /// Runtime-internal completion posted by [`HostCx::enqueue`].
    Enqueued(HostFn),
}

/// A runtime task handle independent of any UI executor.
pub type HostTask<T> = smol::Task<T>;

/// The only execution/event context accepted by [`AppState`] methods.
///
/// Clones are `Send` and may be held by background work. All state mutation
/// returns through `mailbox`; emitted events enter the typed host-output
/// stream; notification-only changes use a bounded channel as a coalescing bit.
#[derive(Clone)]
pub struct HostCx {
    mailbox: smol::channel::Sender<HostMsg>,
    events: smol::channel::Sender<HostMessage>,
    pending: Arc<Mutex<HashMap<u64, smol::channel::Sender<HostMessage>>>>,
    runtime_seq: Arc<Mutex<u64>>,
    changed: smol::channel::Sender<()>,
}

impl HostCx {
    pub(crate) fn new(
        mailbox: smol::channel::Sender<HostMsg>,
        events: smol::channel::Sender<HostMessage>,
        pending: Arc<Mutex<HashMap<u64, smol::channel::Sender<HostMessage>>>>,
        changed: smol::channel::Sender<()>,
    ) -> Self {
        Self {
            mailbox,
            events,
            pending,
            runtime_seq: Arc::new(Mutex::new(0)),
            changed,
        }
    }

    pub fn emit(&mut self, event: HostEvent) {
        let envelope = match event {
            HostEvent::Domain(envelope) => envelope,
            HostEvent::Runtime(notification) => {
                let mut seq = self.runtime_seq.lock().unwrap();
                *seq = seq.wrapping_add(1);
                let envelope = EventEnvelope {
                    topic: Topic::RuntimeEvents,
                    seq: *seq,
                    event: ServerEvent::Runtime(notification),
                };
                let _ = self.events.try_send(HostMessage::Event(envelope));
                return;
            }
        };
        let _ = self.events.try_send(HostMessage::Event(envelope));
    }

    pub(crate) fn send_message(&self, message: HostMessage) {
        let id = match &message {
            HostMessage::Ack { id, .. } | HostMessage::QueryResult { id, .. } => Some(*id),
            _ => None,
        };
        if let Some(id) = id
            && let Some(waiter) = self.pending.lock().unwrap().remove(&id)
        {
            let _ = waiter.try_send(message);
        } else {
            let _ = self.events.try_send(message);
        }
    }

    pub fn notify(&mut self) {
        // Capacity one makes this a coalesced dirty bit. The client-side pump
        // drains it and maps each observation to its UI notification primitive.
        let _ = self.changed.try_send(());
    }

    pub fn spawn_background<T: Send + 'static>(
        &self,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> HostTask<T> {
        smol::spawn(fut)
    }

    pub fn enqueue(&self, f: impl FnOnce(&mut AppState, &mut HostCx) + Send + 'static) {
        let _ = self.mailbox.try_send(HostMsg::Enqueued(Box::new(f)));
    }

    pub(crate) async fn enqueue_and_wait<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut AppState, &mut HostCx) -> R + Send + 'static,
    ) -> Result<R, ()> {
        let (sender, receiver) = smol::channel::bounded(1);
        self.mailbox
            .send(HostMsg::Enqueued(Box::new(move |state, cx| {
                let _ = sender.try_send(f(state, cx));
            })))
            .await
            .map_err(|_| ())?;
        receiver.recv().await.map_err(|_| ())
    }

    pub(crate) fn timer(&self, duration: std::time::Duration) -> HostTask<()> {
        self.spawn_background(async move {
            smol::Timer::after(duration).await;
        })
    }

    pub fn spawn_detached(&self, fut: impl Future<Output = ()> + Send + 'static) {
        self.spawn_background(fut).detach();
    }
}
