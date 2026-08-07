//! Dedicated-thread host execution seam.
//!
//! `AppState` is a plain `Send` value owned by the host loop. UI clients can
//! reach it only through decoded protocol messages. Background completions use
//! the same mailbox via [`HostCx::enqueue`].

use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use tcode_protocol::{ClientMessage, HostMessage};

use crate::app::AppState;
use crate::event::HostEvent;

pub(crate) type HostFn = Box<dyn FnOnce(&mut AppState, &mut HostCx) + Send + 'static>;

/// The single mailbox consumed by the thread that owns [`AppState`].
pub(crate) enum HostMsg {
    /// A client line that has crossed serde/NDJSON decoding successfully.
    DecodedClient(Box<ClientMessage>),
    /// Runtime-internal completion posted by [`HostCx::enqueue`].
    Enqueued(HostFn),
    /// The last serialized client endpoint was dropped.
    ClientClosed,
}

/// Items accepted by the host-side NDJSON encoder.
pub(crate) enum HostOutput {
    Event(HostEvent),
    Message(HostMessage),
}

/// A runtime task handle independent of any UI executor.
#[must_use = "tasks must be awaited or detached"]
pub struct HostTask<T>(smol::Task<T>);

impl<T> HostTask<T> {
    pub(crate) fn new(task: smol::Task<T>) -> Self {
        Self(task)
    }

    /// Keep running the task after dropping its handle.
    pub fn detach(self) {
        self.0.detach();
    }
}

impl<T: 'static> Future for HostTask<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        // `self.0` is structurally pinned with `self`; it is never moved while
        // this `HostTask` is pinned.
        let this = unsafe { self.get_unchecked_mut() };
        unsafe { Pin::new_unchecked(&mut this.0) }.poll(cx)
    }
}

/// The only execution/event context accepted by [`AppState`] methods.
///
/// Clones are `Send` and may be held by background work. All state mutation
/// returns through `mailbox`; emitted events enter the serialized host-output
/// stream; notification-only changes use a bounded channel as a coalescing bit.
#[derive(Clone)]
pub struct HostCx {
    mailbox: async_channel::Sender<HostMsg>,
    outgoing: async_channel::Sender<HostOutput>,
    changed: async_channel::Sender<()>,
}

impl HostCx {
    pub(crate) fn new(
        mailbox: async_channel::Sender<HostMsg>,
        outgoing: async_channel::Sender<HostOutput>,
        changed: async_channel::Sender<()>,
    ) -> Self {
        Self {
            mailbox,
            outgoing,
            changed,
        }
    }

    pub fn emit(&mut self, event: HostEvent) {
        let _ = self.outgoing.try_send(HostOutput::Event(event));
    }

    pub(crate) fn send_message(&self, message: HostMessage) {
        let _ = self.outgoing.try_send(HostOutput::Message(message));
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
        HostTask::new(smol::spawn(fut))
    }

    pub fn enqueue(&self, f: impl FnOnce(&mut AppState, &mut HostCx) + Send + 'static) {
        let _ = self.mailbox.try_send(HostMsg::Enqueued(Box::new(f)));
    }

    pub(crate) async fn enqueue_and_wait<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut AppState, &mut HostCx) -> R + Send + 'static,
    ) -> Result<R, ()> {
        let (sender, receiver) = async_channel::bounded(1);
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
