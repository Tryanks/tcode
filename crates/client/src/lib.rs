//! Transport-agnostic client endpoint for a tcode host.

pub mod pairing;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tcode_protocol::{
    ClientMessage, ClientPayload, Command, CommandResponse, EventEnvelope, HostMessage,
    ProtocolError, Query, QueryResponse, Subscription, Topic, decode_host_line, encode_line,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Reconnecting { attempt: u32 },
    Offline,
}

struct HostLinkInner {
    to_host: async_channel::Sender<String>,
    from_host: async_channel::Receiver<String>,
    pending: Mutex<HashMap<u64, async_channel::Sender<HostMessage>>>,
    events_tx: async_channel::Sender<EventEnvelope>,
    events_rx: async_channel::Receiver<EventEnvelope>,
    next_id: AtomicU64,
    subscribed_topics: Mutex<HashMap<Topic, Subscription>>,
    retired_topics: Mutex<HashSet<Topic>>,
    subscription_requests: Mutex<HashMap<Topic, u64>>,
    connection_state: Mutex<ConnectionState>,
    connection_state_tx: async_channel::Sender<ConnectionState>,
    connection_state_rx: async_channel::Receiver<ConnectionState>,
}

/// A client endpoint whose only transport contract is a pair of NDJSON channels.
#[derive(Clone)]
pub struct HostLink {
    inner: Arc<HostLinkInner>,
}

/// Receiver for decoded host events. Correlated responses never enter this stream.
#[derive(Clone)]
pub struct HostEventReceiver {
    receiver: async_channel::Receiver<EventEnvelope>,
}

pub type CommandFuture =
    Pin<Box<dyn Future<Output = Result<CommandResponse, ProtocolError>> + Send + 'static>>;

#[derive(Debug)]
pub enum HostEventTryRecvError {
    Empty,
    Closed,
}

impl HostEventReceiver {
    pub async fn recv(&self) -> Result<EventEnvelope, ProtocolError> {
        self.receiver.recv().await.map_err(transport_error)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn recv_blocking(&self) -> Result<EventEnvelope, ProtocolError> {
        self.receiver.recv_blocking().map_err(transport_error)
    }

    pub fn try_recv(&self) -> Result<EventEnvelope, HostEventTryRecvError> {
        self.receiver.try_recv().map_err(|error| match error {
            async_channel::TryRecvError::Empty => HostEventTryRecvError::Empty,
            async_channel::TryRecvError::Closed => HostEventTryRecvError::Closed,
        })
    }
}

impl HostLink {
    pub fn new(
        to_host: async_channel::Sender<String>,
        from_host: async_channel::Receiver<String>,
    ) -> Self {
        let (events_tx, events_rx) = async_channel::unbounded();
        let (connection_state_tx, connection_state_rx) = async_channel::unbounded();
        Self {
            inner: Arc::new(HostLinkInner {
                to_host,
                from_host,
                pending: Mutex::new(HashMap::new()),
                events_tx,
                events_rx,
                next_id: AtomicU64::new(1),
                subscribed_topics: Mutex::new(HashMap::new()),
                retired_topics: Mutex::new(HashSet::new()),
                subscription_requests: Mutex::new(HashMap::new()),
                connection_state: Mutex::new(ConnectionState::Connected),
                connection_state_tx,
                connection_state_rx,
            }),
        }
    }

    /// Decode and route host output until its transport channel closes.
    ///
    /// The owner of the transport is responsible for running exactly one pump.
    pub async fn pump(&self) {
        while let Ok(line) = self.inner.from_host.recv().await {
            match decode_host_line(&line) {
                Ok(HostMessage::Event(envelope)) => {
                    if self
                        .inner
                        .subscribed_topics
                        .lock()
                        .unwrap()
                        .contains_key(&envelope.topic)
                    {
                        let _ = self.inner.events_tx.send(envelope).await;
                    }
                }
                Ok(
                    message @ (HostMessage::Ack { id, .. } | HostMessage::QueryResult { id, .. }),
                ) => {
                    if let Some(waiter) = self.inner.pending.lock().unwrap().remove(&id) {
                        let _ = waiter.try_send(message);
                    }
                }
                Err(error) => log::error!("failed to decode host message: {}", error.message),
            }
        }
        self.inner.pending.lock().unwrap().clear();
        self.inner.events_tx.close();
    }

    pub fn next_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn send_payload(&self, id: u64, payload: ClientPayload) -> Result<(), ProtocolError> {
        let line = encode_line(&ClientMessage { id, payload })?;
        self.inner.to_host.try_send(line).map_err(transport_error)
    }

    fn begin_request(
        &self,
        id: u64,
        payload: ClientPayload,
    ) -> Result<async_channel::Receiver<HostMessage>, ProtocolError> {
        let (sender, receiver) = async_channel::bounded(1);
        self.inner.pending.lock().unwrap().insert(id, sender);
        if let Err(error) = self.send_payload(id, payload) {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        Ok(receiver)
    }

    async fn request(&self, payload: ClientPayload) -> Result<HostMessage, ProtocolError> {
        let id = self.next_id();
        self.begin_request(id, payload)?
            .recv()
            .await
            .map_err(transport_error)
    }

    pub fn dispatch(&self, command: Command) -> Result<(), ProtocolError> {
        self.send_payload(self.next_id(), ClientPayload::Command(command))
    }

    pub async fn command(&self, command: Command) -> Result<CommandResponse, ProtocolError> {
        match self.request(ClientPayload::Command(command)).await? {
            HostMessage::Ack { result, .. } => result,
            other => Err(unexpected_response("command ack", &other)),
        }
    }

    pub fn command_with_id(&self, command: Command) -> (u64, CommandFuture) {
        let id = self.next_id();
        let link = self.clone();
        let future = async move {
            let receiver = link.begin_request(id, ClientPayload::Command(command))?;
            match receiver.recv().await.map_err(transport_error)? {
                HostMessage::Ack { result, .. } => result,
                other => Err(unexpected_response("command ack", &other)),
            }
        };
        (id, Box::pin(future))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn command_blocking(&self, command: Command) -> Result<CommandResponse, ProtocolError> {
        let id = self.next_id();
        match self
            .begin_request(id, ClientPayload::Command(command))?
            .recv_blocking()
            .map_err(transport_error)?
        {
            HostMessage::Ack { result, .. } => result,
            other => Err(unexpected_response("command ack", &other)),
        }
    }

    pub async fn query(&self, query: Query) -> Result<QueryResponse, ProtocolError> {
        match self.request(ClientPayload::Query(query)).await? {
            HostMessage::QueryResult { result, .. } => result,
            other => Err(unexpected_response("query result", &other)),
        }
    }

    pub fn subscribe(&self, subscription: Subscription) -> Result<(), ProtocolError> {
        self.inner
            .retired_topics
            .lock()
            .unwrap()
            .remove(&subscription.topic);
        self.inner
            .subscribed_topics
            .lock()
            .unwrap()
            .insert(subscription.topic.clone(), subscription.clone());
        self.send_subscription(subscription)
    }

    fn send_subscription(&self, subscription: Subscription) -> Result<(), ProtocolError> {
        let id = self.next_id();
        self.inner
            .subscription_requests
            .lock()
            .unwrap()
            .insert(subscription.topic.clone(), id);
        self.send_payload(id, ClientPayload::Subscribe(subscription))
    }

    /// A newer cursor supersedes an older subscription reply, including replies
    /// already queued for the UI when it advanced its replica. Check on application,
    /// not just in the transport pump, to avoid a stale tail resetting live records.
    pub fn subscription_reply_is_current(&self, envelope: &EventEnvelope) -> bool {
        envelope.request_id.is_none_or(|id| {
            self.inner
                .subscription_requests
                .lock()
                .unwrap()
                .get(&envelope.topic)
                == Some(&id)
        })
    }

    pub fn unsubscribe(&self, subscription: Subscription) -> Result<(), ProtocolError> {
        self.inner
            .subscribed_topics
            .lock()
            .unwrap()
            .remove(&subscription.topic);
        self.inner
            .retired_topics
            .lock()
            .unwrap()
            .insert(subscription.topic.clone());
        self.send_payload(self.next_id(), ClientPayload::Unsubscribe(subscription))
    }

    /// Advance replay memory only after the store has applied these records. Sending
    /// the latest subscription also updates native/browser transport replay caches.
    /// The host returns an empty tail when the store is already up to date.
    pub fn update_after(&self, topic: &Topic, after: u64) -> Result<(), ProtocolError> {
        let subscription = {
            let mut topics = self.inner.subscribed_topics.lock().unwrap();
            let Some(subscription) = topics.get_mut(topic) else {
                return Ok(());
            };
            if subscription.after == Some(after) {
                return Ok(());
            }
            subscription.after = Some(after);
            subscription.clone()
        };
        self.send_subscription(subscription)
    }

    pub fn subscriptions(&self) -> Vec<Subscription> {
        self.inner
            .subscribed_topics
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    pub fn events(&self) -> HostEventReceiver {
        HostEventReceiver {
            receiver: self.inner.events_rx.clone(),
        }
    }

    pub fn subscribed_topics(&self) -> Vec<Topic> {
        self.inner
            .subscribed_topics
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.inner.connection_state.lock().unwrap().clone()
    }

    pub fn set_connection_state(&self, state: ConnectionState) {
        let previous = std::mem::replace(
            &mut *self.inner.connection_state.lock().unwrap(),
            state.clone(),
        );
        if state == ConnectionState::Connected && previous != ConnectionState::Connected {
            // The store's acknowledged cursor is authoritative even when a transport
            // has already replayed an older line during its own handshake.
            for topic in self.inner.retired_topics.lock().unwrap().iter() {
                let _ = self.send_payload(
                    self.next_id(),
                    ClientPayload::Unsubscribe(Subscription {
                        topic: topic.clone(),
                        after: None,
                    }),
                );
            }
            for subscription in self.subscriptions() {
                let _ = self.send_subscription(subscription);
            }
        }
        let _ = self.inner.connection_state_tx.try_send(state);
    }

    pub fn connection_state_changes(&self) -> async_channel::Receiver<ConnectionState> {
        self.inner.connection_state_rx.clone()
    }

    pub async fn shutdown(&self) -> Result<(), ProtocolError> {
        let result = self.command(Command::ShutdownAllAndFlush).await;
        self.inner.to_host.close();
        match result? {
            CommandResponse::Unit => Ok(()),
            other => Err(ProtocolError {
                code: "unexpected_response".into(),
                message: format!("expected unit shutdown ack, got {other:?}"),
            }),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn shutdown_blocking(&self) -> Result<(), ProtocolError> {
        let result = self.command_blocking(Command::ShutdownAllAndFlush);
        self.inner.to_host.close();
        match result? {
            CommandResponse::Unit => Ok(()),
            other => Err(ProtocolError {
                code: "unexpected_response".into(),
                message: format!("expected unit shutdown ack, got {other:?}"),
            }),
        }
    }
}

fn unexpected_response(expected: &str, actual: &HostMessage) -> ProtocolError {
    ProtocolError {
        code: "unexpected_response".into(),
        message: format!("expected {expected}, got {actual:?}"),
    }
}

fn transport_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError {
        code: "transport_closed".into(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcode_protocol::{IndexSnapshot, ServerEvent};

    #[test]
    fn reconnect_replays_the_cursor_applied_by_the_store_and_retired_topics() {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let topic = Topic::SessionEvents {
            session_id: "one".into(),
        };
        link.subscribe(Subscription {
            topic: topic.clone(),
            after: None,
        })
        .unwrap();
        outgoing.try_recv().unwrap();
        link.update_after(&topic, 7).unwrap();
        let updated = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert!(matches!(
            updated.payload,
            ClientPayload::Subscribe(Subscription { after: Some(7), .. })
        ));
        link.set_connection_state(ConnectionState::Reconnecting { attempt: 1 });
        link.set_connection_state(ConnectionState::Connected);
        let replay = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert_eq!(updated.payload, replay.payload);
        link.unsubscribe(Subscription { topic, after: None })
            .unwrap();
        outgoing.try_recv().unwrap();
        link.set_connection_state(ConnectionState::Reconnecting { attempt: 2 });
        link.set_connection_state(ConnectionState::Connected);
        assert!(matches!(
            tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap())
                .unwrap()
                .payload,
            ClientPayload::Unsubscribe(_)
        ));
        assert!(link.subscribed_topics().is_empty());
    }

    #[test]
    fn correlates_responses_forwards_events_and_remembers_topics() {
        let (to_host, client_lines) = async_channel::unbounded();
        let (host_lines, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let pump = std::thread::spawn({
            let link = link.clone();
            move || smol::block_on(link.pump())
        });
        let server = std::thread::spawn(move || {
            let request = loop {
                let line = client_lines.recv_blocking().unwrap();
                let request = tcode_protocol::decode_client_line(&line).unwrap();
                if matches!(request.payload, ClientPayload::Command(_)) {
                    break request;
                }
            };
            host_lines
                .send_blocking(
                    encode_line(&HostMessage::Ack {
                        id: request.id + 100,
                        result: Ok(CommandResponse::Unit),
                    })
                    .unwrap(),
                )
                .unwrap();
            host_lines
                .send_blocking(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic: Topic::Index,
                        event: ServerEvent::IndexSnapshot(IndexSnapshot {
                            activity: Default::default(),
                            sessions: Vec::new(),
                            projects: Vec::new(),
                        }),
                    }))
                    .unwrap(),
                )
                .unwrap();
            host_lines
                .send_blocking(
                    encode_line(&HostMessage::Ack {
                        id: request.id,
                        result: Ok(CommandResponse::Unit),
                    })
                    .unwrap(),
                )
                .unwrap();
            host_lines.close();
        });

        link.subscribe(Subscription {
            after: None,
            topic: Topic::Index,
        })
        .unwrap();
        link.subscribe(Subscription {
            after: None,
            topic: Topic::Index,
        })
        .unwrap();
        assert_eq!(link.subscribed_topics(), vec![Topic::Index]);
        assert_eq!(
            link.command_blocking(Command::OpenLatestSession).unwrap(),
            CommandResponse::Unit
        );
        assert_eq!(link.events().recv_blocking().unwrap().topic, Topic::Index);
        server.join().unwrap();
        pump.join().unwrap();
    }
}
