use std::rc::Rc;

use gpui::{App, AppContext as _, Entity, Window};
use tcode_client::{HostLink, host::ClientHost};
use tcode_remote::{HostMux, client::PairedHost};
use tcode_ui::{
    AppShell, WindowState,
    overlay::OverlayExt as _,
    remote::AttachmentTarget,
    settings,
    store::{WorkspaceAttachment, WorkspaceStore},
    theme::{self, ThemeMode as UiThemeMode},
};

struct LinkRuntime {
    link: HostLink,
    pump: Option<smol::Task<()>>,
    state_forwarder: Option<smol::Task<()>>,
    pump_finished: async_channel::Receiver<()>,
}

impl LinkRuntime {
    fn local(mux: &HostMux) -> Self {
        let connection = mux.attach();
        Self::start(
            HostLink::new(connection.to_host, connection.from_host),
            None,
        )
    }

    fn remote(client_host: &dyn ClientHost, host: &PairedHost) -> Self {
        let transport = client_host.connect(host);
        let link = HostLink::new(transport.to_host, transport.from_host);
        Self::start(link, Some(transport.state))
    }

    fn start(
        link: HostLink,
        states: Option<async_channel::Receiver<tcode_client::ConnectionState>>,
    ) -> Self {
        let (finished, pump_finished) = async_channel::bounded(1);
        let pump_link = link.clone();
        let pump = smol::spawn(async move {
            pump_link.pump().await;
            let _ = finished.try_send(());
        });
        let state_forwarder = states.map(|states| {
            let link = link.clone();
            smol::spawn(async move {
                while let Ok(state) = states.recv().await {
                    link.set_connection_state(state);
                }
            })
        });
        Self {
            link,
            pump: Some(pump),
            state_forwarder,
            pump_finished,
        }
    }

    /// Close the transport and synchronously join both attachment tasks.
    fn close(mut self) -> bool {
        self.link.close();
        if let Some(task) = self.state_forwarder.take() {
            smol::block_on(task.cancel());
        }
        if let Some(task) = self.pump.take() {
            smol::block_on(task);
        }
        self.pump_finished.try_recv().is_ok()
    }
}

impl Drop for LinkRuntime {
    fn drop(&mut self) {
        self.link.close();
    }
}

pub struct PreparedAttachment {
    target: AttachmentTarget,
    runtime: LinkRuntime,
    pub store: Entity<WorkspaceStore>,
}

struct Attachment {
    target: AttachmentTarget,
    runtime: LinkRuntime,
    store: Entity<WorkspaceStore>,
    shell: Entity<AppShell>,
}

/// Owns the complete view graph and transport tasks for one desktop link.
pub struct DesktopSession {
    mux: HostMux,
    client_host: Rc<dyn ClientHost>,
    window_state: Entity<WindowState>,
    current: Option<Attachment>,
}

impl DesktopSession {
    pub fn new(
        mux: HostMux,
        client_host: Rc<dyn ClientHost>,
        window_state: Entity<WindowState>,
    ) -> Self {
        Self {
            mux,
            client_host,
            window_state,
            current: None,
        }
    }

    pub fn prepare(&self, target: AttachmentTarget, cx: &mut App) -> PreparedAttachment {
        let (runtime, identity) = match &target {
            AttachmentTarget::Local => (LinkRuntime::local(&self.mux), WorkspaceAttachment::Local),
            AttachmentTarget::Remote(host) => (
                LinkRuntime::remote(self.client_host.as_ref(), host),
                WorkspaceAttachment::Remote {
                    host_id: host.host_id.clone(),
                    host_name: host.name.clone(),
                    address: host.addrs.first().cloned(),
                },
            ),
        };
        let link = runtime.link.clone();
        let client_host = self.client_host.clone();
        let store =
            cx.new(|cx| WorkspaceStore::new_attached(link, identity, Some(client_host), true, cx));
        PreparedAttachment {
            target,
            runtime,
            store,
        }
    }

    pub fn mount(
        &mut self,
        prepared: PreparedAttachment,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<AppShell> {
        let store = prepared.store;
        let shell =
            cx.new(|cx| AppShell::new(store.clone(), self.window_state.clone(), window, cx));
        self.remember_target(&prepared.target);
        self.current = Some(Attachment {
            target: prepared.target,
            runtime: prepared.runtime,
            store,
            shell: shell.clone(),
        });
        shell
    }

    pub fn switch_to(&mut self, target: AttachmentTarget, window: &mut Window, cx: &mut App) {
        if self
            .current
            .as_ref()
            .is_some_and(|attachment| same_target(&attachment.target, &target))
        {
            return;
        }

        if let Some(old) = self.current.take() {
            window.detach_view(cx);
            old.store.update(cx, |store, cx| store.detach(cx));
            let pump_finished = old.runtime.close();
            debug_assert!(pump_finished, "detached HostLink pump did not finish");
            drop(old.shell);
            drop(old.store);
        }

        let prepared = self.prepare(target, cx);
        let settings = prepared.store.read(cx).settings();
        settings::apply_locale(settings.language.as_deref());
        match settings.theme_mode {
            settings::ThemeMode::Light => theme::change_mode(UiThemeMode::Light, Some(window), cx),
            settings::ThemeMode::Dark => theme::change_mode(UiThemeMode::Dark, Some(window), cx),
            settings::ThemeMode::System => theme::sync_system_appearance(Some(window), cx),
        }
        let shell = self.mount(prepared, window, cx);
        window.replace_view(shell, cx);
    }

    pub fn store(&self) -> Entity<WorkspaceStore> {
        self.current
            .as_ref()
            .expect("desktop attachment must be mounted")
            .store
            .clone()
    }

    pub fn link(&self) -> HostLink {
        self.current
            .as_ref()
            .expect("desktop attachment must be mounted")
            .runtime
            .link
            .clone()
    }

    fn remember_target(&self, target: &AttachmentTarget) {
        self.client_host.set_last_host_id(match target {
            AttachmentTarget::Local => None,
            AttachmentTarget::Remote(host) => Some(host.host_id.as_str()),
        });
    }
}

fn same_target(left: &AttachmentTarget, right: &AttachmentTarget) -> bool {
    match (left, right) {
        (AttachmentTarget::Local, AttachmentTarget::Local) => true,
        (AttachmentTarget::Remote(left), AttachmentTarget::Remote(right)) => {
            left.host_id == right.host_id
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use tcode_protocol::{
        ClientPayload, Command, EventEnvelope, HostMessage, IndexSnapshot, ServerEvent,
        Subscription, Topic, decode_client_line, encode_line,
    };

    use super::*;

    #[test]
    fn closing_an_attachment_finishes_its_pump_without_shutting_down_the_host() {
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let runtime = LinkRuntime::start(HostLink::new(to_host, from_host), None);
        runtime
            .link
            .subscribe(Subscription {
                topic: Topic::Index,
                after: None,
            })
            .unwrap();

        assert!(runtime.close(), "pump must finish after transport close");
        let stale = encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(IndexSnapshot {
                activity: Default::default(),
                sessions: Vec::new(),
                projects: Vec::new(),
            }),
        }))
        .unwrap();
        assert!(
            incoming.try_send(stale).is_err(),
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

    #[test]
    fn detaching_one_mux_client_leaves_an_independent_client_attached() {
        let (to_host, host_requests) = async_channel::unbounded();
        let (host_events, from_host) = async_channel::unbounded();
        let mux = HostMux::new(to_host, from_host);
        let old = LinkRuntime::local(&mux);
        let second = LinkRuntime::local(&mux);
        second
            .link
            .subscribe(Subscription {
                topic: Topic::Index,
                after: None,
            })
            .unwrap();
        host_requests.recv_blocking().unwrap();

        assert!(old.close());
        host_events
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
        let received = smol::block_on(second.link.events().recv()).unwrap();
        assert_eq!(received.topic, Topic::Index);
        assert!(second.close());
    }
}
