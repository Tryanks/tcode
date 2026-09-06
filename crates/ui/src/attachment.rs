//! The window's attachment to a host: one transport, its pump, and the
//! workspace store built on top of them.
//!
//! The window outlives any single attachment. Switching hosts replaces
//! everything in here and nothing above it, so the guarantees are narrow and
//! worth stating:
//!
//! - **Nothing blocks the window.** Teardown hands back the retiring pump's
//!   completion as a value to await, rather than joining it on the UI thread —
//!   which a browser, running the whole client on one executor, cannot do at
//!   all.
//! - **A retiring task cannot reach the next attachment.** Closing the link
//!   closes its channels first, so anything still in flight is dropped rather
//!   than delivered into the store that replaced it.
//! - **Detaching is not shutting down.** The client's link closes; the host
//!   keeps running, and every other client attached to it keeps working.

use std::rc::Rc;

use gpui::{App, AppContext as _, Entity, Task};
use tcode_client::HostLink;
use tcode_client::host::{ClientHost, Transport};

use crate::remote::AttachmentTarget;
use crate::store::{WorkspaceAttachment, WorkspaceStore};

/// How this window reaches a host running inside its own process. Desktop
/// bootstrap supplies one; a client that can never host has none, so
/// [`AttachmentTarget::Local`] simply has nothing to open.
pub type LocalTransport = Rc<dyn Fn() -> Transport>;

struct AttachmentRuntime {
    link: HostLink,
    finished: async_channel::Receiver<()>,
    tasks: Vec<Task<()>>,
}

impl AttachmentRuntime {
    fn start(transport: Transport, cx: &mut App) -> Self {
        let link = HostLink::new(transport.to_host, transport.from_host);
        let (finished, pump_finished) = async_channel::bounded(1);
        let pump = link.clone();
        let states = transport.state;
        let state_link = link.clone();
        Self {
            finished: pump_finished,
            tasks: vec![
                cx.background_spawn(async move {
                    pump.pump().await;
                    let _ = finished.try_send(());
                }),
                // A local transport has no state channel behind it; the closed
                // receiver ends this task on its first poll.
                cx.background_spawn(async move {
                    while let Ok(state) = states.recv().await {
                        state_link.set_connection_state(state);
                    }
                }),
            ],
            link,
        }
    }

    /// Close the transport and hand back whether its pump finished. The tasks
    /// ride along with the watcher so they can drain, and the closed link keeps
    /// them from delivering anything into the attachment that replaced this one.
    fn close(self, cx: &mut App) -> async_channel::Receiver<bool> {
        self.link.close();
        let (done, observed) = async_channel::bounded(1);
        let finished = self.finished;
        let tasks = self.tasks;
        cx.background_spawn(async move {
            let completed = finished.recv().await.is_ok();
            if !completed {
                log::error!("a detached host link pump was dropped before it finished");
            }
            drop(tasks);
            let _ = done.try_send(completed);
        })
        .detach();
        observed
    }
}

/// One live attachment: where it points, how it talks, and what it replicates.
pub struct Attachment {
    pub target: AttachmentTarget,
    pub store: Entity<WorkspaceStore>,
    runtime: AttachmentRuntime,
}

impl Attachment {
    /// Open a link to `target` and build its store. `None` when this client has
    /// no way to reach that target — a browser asked to attach locally has no
    /// local host to attach to.
    pub fn open(
        target: AttachmentTarget,
        local: Option<&LocalTransport>,
        client_host: Option<Rc<dyn ClientHost>>,
        seed_blocking: bool,
        cx: &mut App,
    ) -> Option<Self> {
        let (transport, identity) = match &target {
            AttachmentTarget::Local => (local?(), WorkspaceAttachment::Local),
            AttachmentTarget::Remote(host) => (
                client_host.as_ref()?.connect(host),
                WorkspaceAttachment::Remote {
                    host_id: host.host_id.clone(),
                    host_name: host.name.clone(),
                    address: host.addrs.first().cloned(),
                },
            ),
        };
        let runtime = AttachmentRuntime::start(transport, cx);
        let link = runtime.link.clone();
        let store = cx
            .new(|cx| WorkspaceStore::new_attached(link, identity, client_host, seed_blocking, cx));
        Some(Self {
            target,
            store,
            runtime,
        })
    }

    pub fn link(&self) -> HostLink {
        self.runtime.link.clone()
    }

    /// Retire this attachment. The returned receiver reports whether the pump
    /// finished; awaiting it is optional, and never on the window's path.
    pub fn close(self, cx: &mut App) -> async_channel::Receiver<bool> {
        self.store.update(cx, |store, cx| store.detach(cx));
        self.runtime.close(cx)
    }
}

/// Whether two targets name the same host. A remote host is identified by its
/// id, not by the address a particular record happens to carry.
pub fn same_target(left: &AttachmentTarget, right: &AttachmentTarget) -> bool {
    match (left, right) {
        (AttachmentTarget::Local, AttachmentTarget::Local) => true,
        (AttachmentTarget::Remote(left), AttachmentTarget::Remote(right)) => {
            left.host_id == right.host_id
        }
        _ => false,
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use gpui::TestAppContext;
    use tcode_protocol::{
        ClientPayload, Command, EventEnvelope, HostMessage, IndexSnapshot, ServerEvent,
        Subscription, Topic, decode_client_line, encode_line,
    };
    use tcode_remote::HostMux;

    use super::*;

    fn index_event() -> String {
        encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(IndexSnapshot {
                activity: Default::default(),
                sessions: Vec::new(),
                projects: Vec::new(),
            }),
        }))
        .unwrap()
    }

    fn transport(
        to_host: async_channel::Sender<String>,
        from_host: async_channel::Receiver<String>,
    ) -> Transport {
        let (_, state) = async_channel::unbounded();
        Transport {
            to_host,
            from_host,
            state,
        }
    }

    /// Leaving a host is not stopping it: the client's own pump ends, its
    /// transport refuses late events, and no shutdown command is ever sent.
    #[gpui::test]
    fn closing_an_attachment_finishes_its_pump_without_shutting_down_the_host(
        cx: &mut TestAppContext,
    ) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let runtime = cx.update(|cx| AttachmentRuntime::start(transport(to_host, from_host), cx));
        runtime
            .link
            .subscribe(Subscription {
                topic: Topic::Index,
                after: None,
            })
            .unwrap();
        cx.run_until_parked();

        let finished = cx.update(|cx| runtime.close(cx));
        cx.run_until_parked();
        assert_eq!(
            finished.try_recv().ok(),
            Some(true),
            "pump must finish after transport close"
        );
        assert!(
            incoming.try_send(index_event()).is_err(),
            "a detached transport must reject late host events"
        );

        let mut lines = Vec::new();
        while let Ok(line) = outgoing.try_recv() {
            lines.push(line);
        }
        assert!(!lines.is_empty());
        assert!(lines.into_iter().all(|line| {
            !matches!(
                decode_client_line(&line).unwrap().payload,
                ClientPayload::Command(Command::ShutdownAllAndFlush)
            )
        }));
    }

    /// Switching this window's attachment must not disturb any other client
    /// sharing the same host.
    ///
    /// `HostMux` pumps on its own OS threads, which the deterministic GPUI test
    /// executor refuses to be woken from, so the two client pumps run on smol
    /// here. What is under test is the pair the runtime above delegates to —
    /// [`HostMux`] and [`HostLink::close`] — not the task wrapper, which the
    /// preceding test covers end to end.
    #[test]
    fn detaching_one_mux_client_leaves_an_independent_client_attached() {
        let (to_host, host_requests) = async_channel::unbounded();
        let (host_events, from_host) = async_channel::unbounded();
        let mux = HostMux::new(to_host, from_host);
        let attach = || {
            let connection = mux.attach();
            let link = HostLink::new(connection.to_host, connection.from_host);
            let pump = link.clone();
            (link, smol::spawn(async move { pump.pump().await }))
        };
        let (old, old_pump) = attach();
        let (second, second_pump) = attach();
        second
            .subscribe(Subscription {
                topic: Topic::Index,
                after: None,
            })
            .unwrap();
        host_requests.recv_blocking().unwrap();

        old.close();
        smol::block_on(old_pump);

        host_events.send_blocking(index_event()).unwrap();
        assert_eq!(
            smol::block_on(second.events().recv()).unwrap().topic,
            Topic::Index,
            "the surviving client must still receive host events"
        );
        second.close();
        smol::block_on(second_pump);
    }
}
