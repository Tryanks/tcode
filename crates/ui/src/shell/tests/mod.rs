use std::cell::RefCell;

use gpui::{TestAppContext, VisualTestContext, size};

use tcode_client::host::Transport;

use tcode_protocol::{
    ClientPayload, Command, EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic,
    decode_client_line, encode_line,
};

use super::*;

mod compact_agent;

mod docked_panel;

mod scenes;

/// One shell over a transport the test holds both ends of, so what the
/// client says to its host is observable and what the transport says
/// about the link can be dictated.
struct MountedShell {
    outgoing: async_channel::Receiver<String>,
    observed: RefCell<Vec<tcode_protocol::ClientMessage>>,
    incoming: async_channel::Sender<String>,
    states: async_channel::Sender<tcode_client::ConnectionState>,
}

struct ReturningClient {
    saved: tcode_client::pairing::PairedHost,
    /// One per attachment the test expects, in order.
    transports: RefCell<Vec<Transport>>,
    preferences: RefCell<tcode_client::host::ClientPreferences>,
    machine_exists: bool,
    outbox: Option<std::sync::Arc<dyn tcode_client::outbox::Storage>>,
}

impl ClientHost for ReturningClient {
    fn outbox_storage(&self, _: &str) -> Option<std::sync::Arc<dyn tcode_client::outbox::Storage>> {
        self.outbox.clone()
    }
    fn device_name(&self) -> String {
        "returning phone".into()
    }

    fn device_id(&self) -> Result<String, tcode_client::host::FileFailure> {
        Ok("returning-phone".into())
    }

    fn device_platform(&self) -> Option<String> {
        None
    }

    fn load_hosts(
        &self,
    ) -> Result<Vec<tcode_client::pairing::PairedHost>, tcode_client::host::FileFailure> {
        Ok(if self.machine_exists {
            vec![self.saved.clone()]
        } else {
            Vec::new()
        })
    }

    fn load_preferences(
        &self,
    ) -> Result<tcode_client::host::ClientPreferences, tcode_client::host::FileFailure> {
        Ok(self.preferences.borrow().clone())
    }

    fn save_preferences(
        &self,
        preferences: &tcode_client::host::ClientPreferences,
    ) -> Result<(), tcode_client::host::FileFailure> {
        *self.preferences.borrow_mut() = preferences.clone();
        Ok(())
    }

    fn save_hosts(
        &self,
        _: &[tcode_client::pairing::PairedHost],
    ) -> Result<(), tcode_client::host::FileFailure> {
        Ok(())
    }

    fn last_host_id(&self) -> Option<String> {
        Some(self.saved.host_id.clone())
    }

    fn set_last_host_id(&self, host_id: Option<&str>) {
        assert_eq!(host_id, Some(self.saved.host_id.as_str()));
    }

    fn pair(
        &self,
        _: tcode_client::pairing::PairInvite,
        _: Option<std::net::IpAddr>,
    ) -> tcode_client::host::HostFuture<
        '_,
        Result<tcode_client::pairing::PairedHost, tcode_client::pairing::PairError>,
    > {
        panic!("a cold start must reuse the saved pairing")
    }

    fn connect(&self, host: &tcode_client::pairing::PairedHost) -> Transport {
        assert_eq!(host.host_id, self.saved.host_id);
        let mut transports = self.transports.borrow_mut();
        assert!(!transports.is_empty(), "an unexpected connection");
        transports.remove(0)
    }
}

/// A returning phone: a mobile build at phone width, so the restored
/// destination is a page of the compact stack.
fn mount_restored<'a>(
    cx: &'a mut TestAppContext,
    history: &[&str],
    machine_exists: bool,
) -> (
    Entity<AppShell>,
    MountedShell,
    Rc<ReturningClient>,
    &'a mut VisualTestContext,
) {
    cx.update(crate::window_seam::force_mobile_layout);
    mount_restored_at_width(cx, history, machine_exists, 393., "plan")
}

fn mount_restored_at_width<'a>(
    cx: &'a mut TestAppContext,
    history: &[&str],
    machine_exists: bool,
    width: f32,
    panel: &str,
) -> (
    Entity<AppShell>,
    MountedShell,
    Rc<ReturningClient>,
    &'a mut VisualTestContext,
) {
    mount_restored_with_outbox(cx, history, machine_exists, width, panel, None)
}

/// [`mount_restored_at_width`] on a device whose outbox for the host
/// holds what `outbox` saved.
fn mount_restored_with_outbox<'a>(
    cx: &'a mut TestAppContext,
    history: &[&str],
    machine_exists: bool,
    width: f32,
    panel: &str,
    outbox: Option<std::sync::Arc<dyn tcode_client::outbox::Storage>>,
) -> (
    Entity<AppShell>,
    MountedShell,
    Rc<ReturningClient>,
    &'a mut VisualTestContext,
) {
    cx.update(crate::test_client::init);
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let (states, state) = async_channel::unbounded();
    let client = Rc::new(ReturningClient {
        saved: tcode_client::pairing::PairedHost {
            host_id: "last-host".into(),
            name: "Last machine".into(),
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["127.0.0.1:47503".into()],
            last_connected_unix: Some(1),
            group_id: None,
            space_id: None,
            space_name: None,
        },
        transports: RefCell::new(vec![Transport {
            originals: None,
            to_host: to_host.into(),
            from_host,
            state,
            current_host: None,
        }]),
        preferences: RefCell::new(tcode_client::host::ClientPreferences {
            navigation: Some(serde_json::json!({
                "history": history,
                "host_id": "last-host",
                "session_id": "thread-a",
                "panel": panel
            })),
            workspace: Some(tcode_client::host::WorkspacePreference {
                hosts: vec![tcode_client::host::WorkspaceHost {
                    host_id: "last-host".into(),
                    collapsed: false,
                }],
                left: Vec::new(),
            }),
            ..Default::default()
        }),
        machine_exists,
        outbox,
    });
    let setup_client = client.clone();
    cx.update(|cx| {
        cx.set_global(crate::remote::ClientAttachment::new(
            setup_client.clone(),
            false,
            request_host,
        ))
    });
    let window = cx.open_window(size(px(width), px(852.)), move |window, cx| {
        let state = cx.new(|_| WindowState::default());
        AppShell::new(
            state,
            ShellSetup {
                initial: crate::last_host_target(setup_client.as_ref()),
                restore_navigation: true,
                ..Default::default()
            },
            window,
            cx,
        )
    });
    let shell = window.root(cx).unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    if let Some(store) = shell.read_with(cx, |shell, cx| shell.store(cx)) {
        crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
    }
    draw(cx);
    (
        shell,
        MountedShell {
            outgoing,
            observed: RefCell::new(Vec::new()),
            incoming,
            states,
        },
        client,
        cx,
    )
}

/// A device that has added `saved` and offers a link to each machine
/// the test holds both ends of: a window over several hosts, none of
/// them in this process.
struct WorkspaceClient {
    saved: RefCell<Vec<tcode_client::pairing::PairedHost>>,
    /// The links to dial, by machine, in order.
    transports: RefCell<HashMap<String, Vec<Transport>>>,
    preferences: RefCell<tcode_client::host::ClientPreferences>,
    last_host_id: RefCell<Option<String>>,
    /// What the next pairing admits this device to.
    pairs: RefCell<Option<tcode_client::pairing::PairedHost>>,
    /// Who hears of a change to `saved`.
    watchers: RefCell<Vec<async_channel::Sender<()>>>,
}

impl ClientHost for WorkspaceClient {
    fn device_name(&self) -> String {
        "tablet".into()
    }

    fn device_id(&self) -> Result<String, tcode_client::host::FileFailure> {
        Ok("tablet".into())
    }

    fn device_platform(&self) -> Option<String> {
        None
    }

    fn load_preferences(
        &self,
    ) -> Result<tcode_client::host::ClientPreferences, tcode_client::host::FileFailure> {
        Ok(self.preferences.borrow().clone())
    }

    fn save_preferences(
        &self,
        preferences: &tcode_client::host::ClientPreferences,
    ) -> Result<(), tcode_client::host::FileFailure> {
        *self.preferences.borrow_mut() = preferences.clone();
        Ok(())
    }

    fn load_hosts(
        &self,
    ) -> Result<Vec<tcode_client::pairing::PairedHost>, tcode_client::host::FileFailure> {
        Ok(self.saved.borrow().clone())
    }

    fn save_hosts(
        &self,
        hosts: &[tcode_client::pairing::PairedHost],
    ) -> Result<(), tcode_client::host::FileFailure> {
        *self.saved.borrow_mut() = hosts.to_vec();
        for watcher in self.watchers.borrow().iter() {
            let _ = watcher.try_send(());
        }
        Ok(())
    }

    fn saved_hosts_changes(&self) -> Option<async_channel::Receiver<()>> {
        let (watcher, changes) = async_channel::unbounded();
        self.watchers.borrow_mut().push(watcher);
        Some(changes)
    }

    fn last_host_id(&self) -> Option<String> {
        self.last_host_id.borrow().clone()
    }

    fn set_last_host_id(&self, host_id: Option<&str>) {
        *self.last_host_id.borrow_mut() = host_id.map(str::to_owned);
    }

    fn pair(
        &self,
        _: tcode_client::pairing::PairInvite,
        _: Option<std::net::IpAddr>,
    ) -> tcode_client::host::HostFuture<
        '_,
        Result<tcode_client::pairing::PairedHost, tcode_client::pairing::PairError>,
    > {
        let paired = self
            .pairs
            .borrow_mut()
            .take()
            .expect("an invitation expected");
        Box::pin(async move { Ok(paired) })
    }

    fn connect(&self, host: &tcode_client::pairing::PairedHost) -> Transport {
        let mut transports = self.transports.borrow_mut();
        let offered = transports.entry(host.host_id.clone()).or_default();
        assert!(
            !offered.is_empty(),
            "an unexpected connection to {}",
            host.host_id
        );
        offered.remove(0)
    }
}

/// The machine's end of one link of a [`WorkspaceClient`].
struct WorkspaceHost {
    host: MountedShell,
    wakes: async_channel::Receiver<tcode_client::recovery::Wake>,
}

impl WorkspaceHost {
    /// Offer `client` a link to `host_id`, the next it dials there.
    fn offer(client: &WorkspaceClient, host_id: &str, cx: &mut TestAppContext) -> Self {
        let (to_host, requests) = tcode_client::outgoing::channel();
        let (forward, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let (states, state) = async_channel::unbounded();
        let wakes = requests.wake.clone();
        // The machine reads every line the window sends it.
        cx.executor()
            .spawn(async move {
                while let Ok(line) = requests.recv().await {
                    if tcode_client::outgoing::subscription_key(&line).is_none() {
                        requests.sent(&line);
                    }
                    if forward.send(line).await.is_err() {
                        break;
                    }
                }
            })
            .detach();
        client
            .transports
            .borrow_mut()
            .entry(host_id.into())
            .or_default()
            .push(Transport {
                originals: None,
                to_host,
                from_host,
                state,
                current_host: None,
            });
        Self {
            host: MountedShell {
                outgoing,
                observed: RefCell::new(Vec::new()),
                incoming,
                states,
            },
            wakes,
        }
    }

    /// The machine admits the window to all of it, holds `projects` and
    /// no thread, and its link is up.
    fn come_up(
        &self,
        store: &Entity<WorkspaceStore>,
        projects: Vec<tcode_core::project::Project>,
        last_project_id: Option<&str>,
        cx: &mut VisualTestContext,
    ) {
        crate::store::tests::seed_full_scope(store, &self.host.incoming, Vec::new(), cx);
        send_events(
            &self.host,
            vec![
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                        last_project_id: last_project_id.map(str::to_owned),
                        ..Default::default()
                    }),
                ),
                (
                    Topic::Index,
                    ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        sessions: Vec::new(),
                        projects,
                    }),
                ),
            ],
        );
        self.host
            .states
            .try_send(tcode_client::ConnectionState::Connected { path: None })
            .unwrap();
        draw(cx);
        cx.run_until_parked();
        draw(cx);
    }
}

fn saved_machine(host_id: &str, group: bool) -> tcode_client::pairing::PairedHost {
    tcode_client::pairing::PairedHost {
        host_id: host_id.into(),
        name: host_id.into(),
        traverse: Vec::new(),
        relay: None,
        addrs: vec!["127.0.0.1:47503".into()],
        last_connected_unix: None,
        group_id: group.then(|| "group".into()),
        space_id: None,
        space_name: None,
    }
}

/// A tablet `width` wide that has added `saved` and holds
/// `preferences`, with a link offered to each machine saved.
fn mount_workspace(
    cx: &mut TestAppContext,
    saved: Vec<tcode_client::pairing::PairedHost>,
    preferences: tcode_client::host::ClientPreferences,
    width: f32,
) -> (
    Entity<AppShell>,
    HashMap<HostKey, WorkspaceHost>,
    Rc<WorkspaceClient>,
    &mut VisualTestContext,
) {
    cx.update(crate::test_client::init);
    cx.update(crate::window_seam::force_mobile_layout);
    let client = Rc::new(WorkspaceClient {
        saved: RefCell::new(saved.clone()),
        transports: RefCell::default(),
        preferences: RefCell::new(preferences),
        last_host_id: RefCell::default(),
        pairs: RefCell::default(),
        watchers: RefCell::default(),
    });
    let hosts: HashMap<HostKey, WorkspaceHost> = saved
        .iter()
        .map(|host| {
            (
                HostKey::Remote(host.host_id.clone()),
                WorkspaceHost::offer(&client, &host.host_id, cx),
            )
        })
        .collect();
    let setup_client: Rc<dyn ClientHost> = client.clone();
    cx.update(|cx| {
        cx.set_global(crate::remote::ClientAttachment::new(
            setup_client.clone(),
            false,
            request_host,
        ))
    });
    let mounted = Rc::new(RefCell::new(None));
    let capture = mounted.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let window_state = cx.new(|_| WindowState::default());
        let shell = cx.new(|cx| {
            AppShell::new(
                window_state,
                ShellSetup {
                    // As every mobile build does.
                    restore_navigation: true,
                    ..Default::default()
                },
                window,
                cx,
            )
        });
        *capture.borrow_mut() = Some(shell.clone());
        gpui_base::Root::new(shell, window, cx)
    });
    let shell: Entity<AppShell> = mounted.borrow_mut().take().unwrap();
    cx.update(|window, cx| {
        window.activate_window();
        set_back_target(window.window_handle(), &shell, cx);
    });
    resize(cx, width);
    (shell, hosts, client, cx)
}

/// The store of `host`, which the window is linked to.
fn store_on(
    shell: &Entity<AppShell>,
    host: &str,
    cx: &VisualTestContext,
) -> Entity<WorkspaceStore> {
    let key = HostKey::Remote(host.into());
    shell.read_with(cx, |shell, _| {
        shell.link_of(&key).expect("linked").store.clone()
    })
}

/// The hosts the window is linked to, in order.
fn linked(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Vec<HostKey> {
    shell.read_with(cx, |shell, _| shell.links.iter().map(Link::key).collect())
}

/// An invitation the pairing form takes; the device's `pair` decides
/// which machine it admits it to.
const INVITATION: &str = "tcode://pair?v=3&id=a5f3c6ea6ba6c5c5d0c4e2b7a5f3c6ea6ba6c5c5d0c4e2b7a5f3c6ea6ba6c5c5&secret=AAECAwQFBgcICQoLDA0ODw&traverse=off&port=47420";

/// Paste `INVITATION` on the pairing page as the user would, the
/// device answering with `admitted`.
fn pair_with(
    shell: &Entity<AppShell>,
    client: &WorkspaceClient,
    admitted: tcode_client::pairing::PairedHost,
    cx: &mut VisualTestContext,
) {
    *client.pairs.borrow_mut() = Some(admitted);
    if shell.read_with(cx, |shell, cx| shell.destination(cx)) != Destination::Pair {
        shell.update(cx, |shell, cx| shell.go(Destination::Pair, cx));
    }
    draw(cx);
    cx.simulate_input(INVITATION);
    draw(cx);
    assert!(client.pairs.borrow().is_none(), "the invitation is sent");
}

fn two_hosts() -> Vec<tcode_client::pairing::PairedHost> {
    vec![saved_machine("host-a", true), saved_machine("host-b", true)]
}

/// Two hosts up at `width`, each listing `thread-1` and `thread-2`, with
/// A's `thread-1` shown in the window's one pane.
fn a_thread_of_a_beside_b(
    cx: &mut TestAppContext,
    width: f32,
) -> (
    Entity<AppShell>,
    HashMap<HostKey, WorkspaceHost>,
    &mut VisualTestContext,
) {
    let (shell, hosts, _client, cx) = mount_workspace(cx, two_hosts(), Default::default(), width);
    cx.update(|_, cx| crate::shortcut::init(cx));
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    for (host, store) in [("host-a", &a), ("host-b", &b)] {
        hosts[&HostKey::Remote(host.into())].come_up(store, Vec::new(), None, cx);
    }
    for host in ["host-a", "host-b"] {
        send_events(
            &hosts[&HostKey::Remote(host.into())].host,
            vec![panes_index(&[])],
        );
    }
    serve_thread(&hosts[&HostKey::Remote("host-a".into())].host, "thread-1");
    a.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx)
    });
    cx.run_until_parked();
    crate::test_frames::draw_settled(cx);
    (shell, hosts, cx)
}

/// The host of each pane, in the layout's order.
fn pane_hosts(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Vec<HostKey> {
    shell.read_with(cx, |shell, cx| {
        let column = shell.column.as_ref().expect("attached");
        column
            .all_panes(cx)
            .iter()
            .map(|pane| pane.read(cx).host(cx))
            .collect()
    })
}

/// The host each pane's header names, in the layout's order.
fn named_hosts(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Vec<Option<SharedString>> {
    shell.read_with(cx, |shell, cx| {
        let column = shell.column.as_ref().expect("attached");
        column
            .all_panes(cx)
            .iter()
            .map(|pane| {
                column
                    .handed
                    .get(&pane.read(cx).chat().entity_id())
                    .and_then(|handed| handed.host.clone())
            })
            .collect()
    })
}

/// Give the keyboard to `pane`'s composer with a tap on it. Each pane of
/// a row has its composer at the same height, the one card the frame
/// names.
fn type_into(pane: &Entity<ThreadPanel>, cx: &mut VisualTestContext) {
    let bounds = pane.read_with(cx, |pane, _| pane.bounds());
    let card = cx.debug_bounds("composer-card").expect("a composer");
    cx.simulate_click(
        gpui::point(bounds.center().x, card.center().y),
        gpui::Modifiers::default(),
    );
    draw(cx);
}

/// The turns `host` was asked to send, by thread.
fn turns_sent(host: &MountedShell) -> Vec<(String, String)> {
    sent(host)
        .into_iter()
        .filter_map(|payload| match payload {
            ClientPayload::Command(Command::SendTurn {
                session_id, text, ..
            }) => Some((session_id, text)),
            _ => None,
        })
        .collect()
}

/// The threads a line typed now and sent with Enter goes to through
/// `host`: none when no composer holds the keyboard.
fn typed_reaches(host: &MountedShell, cx: &mut VisualTestContext) -> Vec<String> {
    const TYPED: &str = "typed line";
    let _ = sent(host);
    cx.simulate_input(TYPED);
    cx.simulate_keystrokes("enter");
    draw(cx);
    turns_sent(host)
        .into_iter()
        .map(|(thread, text)| {
            assert_eq!(text, TYPED, "the line alone is sent");
            thread
        })
        .collect()
}

/// Both hosts of [`a_thread_of_a_beside_b`] keep its two threads in a
/// project, as the palette lists a host's threads by project.
fn threads_in_a_project(hosts: &HashMap<HostKey, WorkspaceHost>, cx: &mut VisualTestContext) {
    let project = tcode_core::project::Project::from_root(PANES_CWD.into());
    let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
        unreachable!("an index");
    };
    for meta in &mut index.sessions {
        meta.project_id = Some(project.id.clone());
    }
    index.projects = vec![project];
    for host in hosts.values() {
        send_events(
            &host.host,
            vec![(topic.clone(), ServerEvent::IndexSnapshot(index.clone()))],
        );
        draw(cx);
        heard(host, cx);
    }
}

/// Open the palette and type `query` into it.
fn search_palette(shell: &Entity<AppShell>, query: &str, cx: &mut VisualTestContext) {
    shell
        .read_with(cx, |shell, _| shell.window_state())
        .update(cx, |state, cx| state.open_palette(cx));
    crate::test_frames::draw_settled(cx);
    cx.simulate_input(query);
    draw(cx);
}

fn click(selector: &'static str, cx: &mut VisualTestContext) {
    let at = cx.debug_bounds(selector).expect(selector).center();
    cx.simulate_click(at, gpui::Modifiers::default());
    draw(cx);
}

/// `store` takes in what `host` sent it, and what it sent `host` so far
/// is set aside.
fn heard(host: &WorkspaceHost, cx: &mut VisualTestContext) {
    cx.run_until_parked();
    draw(cx);
    let _ = sent(&host.host);
}

/// The new threads `host` is asked for: each project's draft, by
/// project, and a quick thread as `None`.
fn drafts_asked(host: &WorkspaceHost) -> Vec<Option<String>> {
    sent(&host.host)
        .into_iter()
        .filter_map(|payload| match payload {
            ClientPayload::Command(Command::StartDraft { project_id, .. }) => {
                Some(Some(project_id))
            }
            ClientPayload::Command(Command::StartScratchDraft) => Some(None),
            _ => None,
        })
        .collect()
}

/// The host names `draft` as the draft of `project` the client asked for.
fn ack_project_draft(host: &MountedShell, project: &str, draft: &str) {
    let request = std::iter::from_fn(|| host.outgoing.try_recv().ok())
        .map(|line| decode_client_line(&line).expect("client line"))
        .find(|message| {
            matches!(
                &message.payload,
                ClientPayload::Command(Command::StartDraft { project_id, .. })
                    if project_id == project
            )
        })
        .expect("the project's draft was asked for");
    host.incoming
        .try_send(
            encode_line(&HostMessage::Ack {
                id: request.id,
                result: Ok(tcode_protocol::CommandResponse::SessionId(Some(
                    draft.into(),
                ))),
            })
            .unwrap(),
        )
        .unwrap();
}

fn await_restore_update(
    shell: &Entity<AppShell>,
    cx: &mut VisualTestContext,
    ready: impl Fn(&WorkspaceStore, &App) -> bool,
) {
    let store = store_of(shell, cx);
    for _ in 0..500 {
        cx.run_until_parked();
        draw(cx);
        if store.read_with(cx, |store, cx| ready(store, cx)) {
            return;
        }
    }
    panic!("restore host update never arrived");
}

fn restore_index(
    shell: &Entity<AppShell>,
    host: &MountedShell,
    present: bool,
    cx: &mut VisualTestContext,
) {
    while let Ok(line) = host.outgoing.try_recv() {
        host.observed
            .borrow_mut()
            .push(decode_client_line(&line).unwrap());
    }
    let scope_request = host
        .observed
        .borrow()
        .iter()
        .rev()
        .find_map(|message| {
            matches!(
                message.payload,
                ClientPayload::Subscribe(tcode_protocol::Subscription {
                    topic: Topic::Scope,
                    ..
                })
            )
            .then_some(message.id)
        })
        .expect("Scope subscription");
    crate::store::tests::seed_full_scope_reply(
        &store_of(shell, cx),
        &host.incoming,
        scope_request,
        Vec::new(),
        cx,
    );
    let mut meta =
        tcode_core::project::SessionMeta::new(agent::ProviderKind::Codex, "/project".into(), None);
    meta.id = "thread-a".into();
    host.incoming
        .try_send(
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::Index,
                event: ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: if present { vec![meta] } else { Vec::new() },
                    projects: Vec::new(),
                }),
            }))
            .unwrap(),
        )
        .unwrap();
    await_restore_update(shell, cx, |store, _| store.index_hydrated());
}

fn restore_status(shell: &Entity<AppShell>, host: &MountedShell, cx: &mut VisualTestContext) {
    host.incoming
        .try_send(
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: "thread-a".into(),
                },
                event: ServerEvent::SessionStatusReplaced(Box::new(session_status(
                    "thread-a",
                    std::path::Path::new("/project"),
                ))),
            }))
            .unwrap(),
        )
        .unwrap();
    await_restore_update(shell, cx, |store, cx| {
        store
            .chat_active_session_for(Some("thread-a"), cx)
            .is_some()
    });
}

/// The rest of the baseline a restored thread waits for, after
/// [`restore_index`] and [`restore_status`].
fn restore_settings_and_events(
    shell: &Entity<AppShell>,
    host: &MountedShell,
    cx: &mut VisualTestContext,
) {
    // The host's language is applied with its settings; the assertions
    // read English.
    let settings = tcode_core::settings::Settings {
        language: Some(crate::LANGUAGE_ENGLISH.into()),
        ..Default::default()
    };
    for (topic, event) in [
        (Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
        (
            Topic::SessionPlan {
                session_id: "thread-a".into(),
            },
            ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                session_id: "thread-a".into(),
                steps: Vec::new(),
                explanation: None,
            }),
        ),
        (
            Topic::SessionEvents {
                session_id: "thread-a".into(),
            },
            ServerEvent::SessionSnapshot {
                from: 0,
                end: 0,
                records: Vec::new(),
                total: 0,
                total_turns: 0,
                truncated: false,
            },
        ),
    ] {
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }
    await_restore_update(shell, cx, |store, _| store.baseline_ready());
}

/// Select `session` with the host describing it: a native subagent's
/// thread, which the host makes read-only and which is running, or an
/// ordinary one.
fn deliver_thread(
    shell: &Entity<AppShell>,
    host: &MountedShell,
    session: &str,
    subagent: bool,
    cx: &mut VisualTestContext,
) {
    let mut meta = tcode_core::project::SessionMeta::new(
        agent::ProviderKind::Codex,
        "/project".into(),
        Some("child-model".into()),
    );
    meta.id = session.into();
    meta.native_subagent = subagent.then(|| "spawn-1".into());
    let mut status = session_status(session, std::path::Path::new("/project"));
    status.requested_model = meta.model.clone();
    status.activity.turn_running = subagent;
    status.conversation_read_only = subagent;
    status
        .provider_option_descriptors
        .push(agent::OptionDescriptor::Select {
            id: "reasoningEffort".into(),
            label: "Reasoning effort".into(),
            options: vec![agent::SelectOption {
                value: "high".into(),
                label: "High".into(),
                description: None,
                unavailable: None,
                speed: None,
                summary: None,
                approval: None,
            }],
            default_value: None,
            role: agent::OptionRole::Effort,
            apply: Default::default(),
            minimum: None,
            recommended: None,
            permissive: None,
        });
    status.provider_option_selections = vec![agent::OptionSelection {
        id: "reasoningEffort".into(),
        value: serde_json::json!("high"),
    }];
    store_of(shell, cx).update(cx, |store, cx| store.show_thread(session.into(), cx));
    for (topic, event) in [
        (
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        ),
        (
            Topic::Index,
            ServerEvent::IndexUpsertSession(Box::new(meta)),
        ),
        (
            Topic::SessionStatus {
                session_id: session.into(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        ),
        (
            Topic::SessionPlan {
                session_id: session.into(),
            },
            ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                session_id: session.into(),
                steps: Vec::new(),
                explanation: None,
            }),
        ),
        (
            Topic::SessionEvents {
                session_id: session.into(),
            },
            ServerEvent::SessionSnapshot {
                total: 0,
                total_turns: 0,
                truncated: false,
                from: 0,
                end: 0,
                records: vec![],
            },
        ),
    ] {
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }
    // The app applies what the host sent as it arrives; a thread come
    // back to draws from what was kept until its window lands.
    await_restore_update(shell, cx, |store, cx| {
        !store.chat_loading_for(Some(session), cx) && store.timeline_settled_for(session)
    });
}

/// A wide window over two indexed threads, `thread-1` (listed first)
/// shown in its only pane, the keyboard in it, its transitions done.
fn mount_panes(
    cx: &mut TestAppContext,
) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
    let (shell, host, cx) = mount(cx);
    cx.update(|window, cx| {
        // Focus listeners run only for an active window, as on screen.
        window.activate_window();
        crate::shortcut::init(cx);
        set_back_target(window.window_handle(), &shell, cx);
    });
    resize(cx, 2000.);
    send_events(
        &host,
        vec![
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            panes_index(&[]),
        ],
    );
    serve_thread(&host, "thread-1");
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx)
    });
    // The window settles before a pointer reaches it.
    crate::test_frames::draw_settled(cx);
    (shell, host, cx)
}

const PANES_CWD: &str = "/tmp/tcode-panes";

fn send_events(host: &MountedShell, events: Vec<(Topic, ServerEvent)>) {
    for (topic, event) in events {
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }
}

/// The index of [`mount_panes`]' two threads, `thread-1` listed first,
/// with `archived` archived.
fn panes_index(archived: &[&str]) -> (Topic, ServerEvent) {
    let sessions = [("thread-1", 2), ("thread-2", 1)].map(|(id, updated_at)| {
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            PANES_CWD.into(),
            None,
        );
        meta.id = id.into();
        meta.title = id.into();
        meta.updated_at = updated_at;
        meta.archived_at = archived.contains(&id).then_some(3);
        meta
    });
    (
        Topic::Index,
        ServerEvent::IndexSnapshot(IndexSnapshot {
            summary: Default::default(),
            sessions: sessions.to_vec(),
            projects: Vec::new(),
        }),
    )
}

fn serve_status(host: &MountedShell, id: &str, draft: bool) {
    let mut status = session_status(id, std::path::Path::new(PANES_CWD));
    status.draft = draft;
    send_events(
        host,
        vec![(
            Topic::SessionStatus {
                session_id: id.into(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        )],
    );
}

/// What the host answers a client showing `id`'s preview while no other
/// client renders it: its pages are this client's.
#[cfg(preview_backend)]
fn render_preview_here(host: &MountedShell, id: &str, cx: &mut VisualTestContext) {
    while let Ok(line) = host.outgoing.try_recv() {
        host.observed
            .borrow_mut()
            .push(decode_client_line(&line).unwrap());
    }
    let client = host
        .observed
        .borrow()
        .iter()
        .rev()
        .find_map(|message| match &message.payload {
            ClientPayload::Subscribe(tcode_protocol::Subscription {
                topic: Topic::Preview { client },
                ..
            }) => Some(client.clone()),
            _ => None,
        })
        .expect("the client offers to render previews");
    let session = tcode_protocol::PreviewSession {
        session_id: id.into(),
        renderer: Some(client.clone()),
        ..Default::default()
    };
    send_events(
        host,
        vec![
            (
                Topic::PreviewSession {
                    session_id: id.into(),
                },
                ServerEvent::PreviewSessionReplaced(session.clone()),
            ),
            (
                Topic::Preview { client },
                ServerEvent::PreviewRenderingReplaced(vec![session]),
            ),
        ],
    );
    cx.run_until_parked();
    cx.run_until_parked();
}

/// What the host answers a thread's subscriptions with: its status and an
/// empty log.
fn serve_thread(host: &MountedShell, id: &str) {
    serve_status(host, id, false);
    send_events(
        host,
        vec![(
            Topic::SessionEvents {
                session_id: id.into(),
            },
            ServerEvent::SessionSnapshot {
                total: 0,
                total_turns: 0,
                truncated: false,
                from: 0,
                end: 0,
                records: Vec::new(),
            },
        )],
    );
}

/// The thread each pane on screen shows, a closing one too, in the
/// layout's order.
fn pane_threads(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Vec<Option<String>> {
    shell.read_with(cx, |shell, cx| {
        shell
            .column
            .as_ref()
            .expect("attached")
            .all_panes(cx)
            .iter()
            .map(|pane| pane.read(cx).session_id().map(str::to_owned))
            .collect()
    })
}

/// Which pane holds the keyboard.
fn keyboard_pane(shell: &Entity<AppShell>, cx: &mut VisualTestContext) -> Option<usize> {
    cx.update(|window, cx| {
        shell
            .read(cx)
            .column
            .as_ref()
            .expect("attached")
            .all_panes(cx)
            .iter()
            .position(|pane| pane.focus_handle(cx).contains_focused(window, cx))
    })
}

/// Where each pane on screen was painted, in the layout's order.
fn pane_bounds(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Vec<gpui::Bounds<Pixels>> {
    shell.read_with(cx, |shell, cx| {
        let attachment = shell.column.as_ref().expect("attached");
        attachment
            .all_panes(cx)
            .iter()
            .map(|pane| pane.read(cx).bounds())
            .collect()
    })
}

/// Answer the scratch draft the client asked for with `draft`, and serve
/// it.
fn answer_draft(
    shell: &Entity<AppShell>,
    host: &MountedShell,
    draft: &str,
    cx: &mut VisualTestContext,
) {
    ack_draft(host, draft);
    await_restore_update(shell, cx, |store, _| store.is_thread_open(draft));
    serve_thread(host, draft);
    await_restore_update(shell, cx, |store, cx| {
        !store.chat_loading_for(Some(draft), cx)
    });
    draw(cx);
}

/// The host names `draft` as the scratch draft the client asked for.
fn ack_draft(host: &MountedShell, draft: &str) {
    let request = std::iter::from_fn(|| host.outgoing.try_recv().ok())
        .map(|line| decode_client_line(&line).expect("client line"))
        .find(|message| {
            matches!(
                message.payload,
                ClientPayload::Command(Command::StartScratchDraft)
            )
        })
        .expect("a draft was asked for");
    host.incoming
        .try_send(
            encode_line(&HostMessage::Ack {
                id: request.id,
                result: Ok(tcode_protocol::CommandResponse::SessionId(Some(
                    draft.into(),
                ))),
            })
            .unwrap(),
        )
        .unwrap();
}

/// Alt-click the threads list's `row`, showing `id`, which opens it in a
/// new pane, and serve it.
fn open_beside_from_list(
    shell: &Entity<AppShell>,
    host: &MountedShell,
    row: &'static str,
    id: &str,
    cx: &mut VisualTestContext,
) {
    let row = cx.debug_bounds(row).expect("the thread's row");
    let alt = gpui::Modifiers {
        alt: true,
        ..Default::default()
    };
    cx.simulate_mouse_move(row.center(), None, alt);
    draw(cx);
    cx.simulate_click(row.center(), alt);
    draw(cx);
    serve_thread(host, id);
    await_restore_update(shell, cx, |store, cx| !store.chat_loading_for(Some(id), cx));
    // The pane glides in before the pointer reaches it.
    crate::test_frames::draw_settled(cx);
}

/// Press on the threads list's `row` and carry it to `to`, drawing as the
/// pointer moves; the caller releases it.
fn drag_row(row: &'static str, to: gpui::Point<Pixels>, cx: &mut VisualTestContext) {
    let from = cx.debug_bounds(row).expect("the thread's row").center();
    let none = gpui::Modifiers::default();
    cx.simulate_mouse_move(from, None, none);
    draw(cx);
    cx.simulate_mouse_down(from, MouseButton::Left, none);
    draw(cx);
    for step in [from + gpui::point(px(20.), px(0.)), (from + to) / 2., to] {
        cx.simulate_mouse_move(step, Some(MouseButton::Left), none);
        draw(cx);
    }
    // The drop's placeholder settles where the pointer rests.
    crate::test_frames::draw_settled(cx);
}

/// Where the column's only pane was painted.
fn only_pane(shell: &Entity<AppShell>, cx: &VisualTestContext) -> gpui::Bounds<Pixels> {
    shell.read_with(cx, |shell, cx| {
        let panes = shell.column.as_ref().expect("attached").all_panes(cx);
        assert_eq!(panes.len(), 1);
        panes[0].read(cx).bounds()
    })
}

/// `id`'s log as the host answers it: a turn still running, in which it
/// asked for each of `spawns`' native subagents.
#[cfg(not(target_os = "windows"))]
fn serve_spawning(host: &MountedShell, id: &str, spawns: &[&str]) {
    let records: Vec<tcode_core::session::StoredEvent> = [
        agent::AgentEvent::TurnStarted {
            turn_id: format!("{id}-turn"),
        },
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: format!("{id}-ask"),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: "Look around".into(),
                attachments: vec![],
            },
        }),
    ]
    .into_iter()
    .chain(spawns.iter().map(|spawn| {
        agent::AgentEvent::ItemStarted(agent::ThreadItem {
            id: (*spawn).into(),
            parent_item_id: None,
            content: agent::ItemContent::Subagent {
                agent_type: "explore".into(),
                description: format!("Survey for {spawn}"),
                status: agent::ItemStatus::InProgress,
                summary: None,
                model: None,
                effort: None,
            },
        })
    }))
    .map(Into::into)
    .collect();
    serve_status(host, id, false);
    send_events(
        host,
        vec![(
            Topic::SessionEvents {
                session_id: id.into(),
            },
            ServerEvent::SessionSnapshot {
                total: records.len() as u64,
                total_turns: 1,
                truncated: false,
                from: 0,
                end: records.len() as u64,
                records,
            },
        )],
    );
}

/// [`mount_panes`] with `agent-1` dispatched by `thread-1`.
fn mount_lead_with_agent(
    cx: &mut TestAppContext,
) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
    let (shell, host, cx) = mount_panes(cx);
    send_events(&host, vec![lead_with_agent_index("thread-1")]);
    await_restore_update(&shell, cx, |store, _| {
        store.thread_meta("agent-1").is_some()
    });
    draw(cx);
    (shell, host, cx)
}

/// The panes' index with `agent-1` dispatched by `thread-1`, which is
/// titled `lead_title`.
fn lead_with_agent_index(lead_title: &str) -> (Topic, ServerEvent) {
    let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
        unreachable!("panes_index sends a snapshot");
    };
    index.sessions[0].title = lead_title.into();
    let mut agent = index.sessions[1].clone();
    agent.id = "agent-1".into();
    agent.title = "agent-1".into();
    agent.parent_session_id = Some("thread-1".into());
    agent.created_at = 3;
    index.sessions.push(agent);
    (topic, ServerEvent::IndexSnapshot(index))
}

/// `id` finishes a turn that changed `file`.
fn serve_changed_turn(host: &MountedShell, id: &str, file: &str) {
    let turn_id = format!("turn-{file}");
    let events = [
        agent::AgentEvent::TurnStarted {
            turn_id: turn_id.clone(),
        },
        agent::AgentEvent::TurnChangesUpdated {
            turn_id: turn_id.clone(),
            changes: agent::file_changes_from_unified_diff(&format!(
                "diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n\
                     @@ -1 +1 @@\n-fn old() {{}}\n+fn new() {{}}\n",
            ))
            .changes,
            completeness: agent::ChangeCompleteness::Exact,
        },
        agent::AgentEvent::TurnCompleted {
            turn_id,
            status: agent::TurnStatus::Completed,
            usage: None,
        },
    ];
    send_events(
        host,
        events
            .into_iter()
            .map(|event| {
                (
                    Topic::SessionEvents {
                        session_id: id.into(),
                    },
                    ServerEvent::SessionEvent(event.into()),
                )
            })
            .collect(),
    );
}

/// Open `agent-1` from its lead's Agents panel, and serve it with an
/// empty log.
fn open_agent_from_panel(
    shell: &Entity<AppShell>,
    host: &MountedShell,
    cx: &mut VisualTestContext,
) {
    let row = cx
        .debug_bounds("agent-row-agent-1")
        .expect("the agent's row in the Agents panel");
    cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::default());
    draw(cx);
    cx.simulate_click(row.center(), gpui::Modifiers::default());
    draw(cx);
    serve_thread(host, "agent-1");
    // The app applies what the host sent as it arrives; an agent come
    // back to draws from what was kept until its window lands.
    await_restore_update(shell, cx, |store, cx| {
        !store.chat_loading_for(Some("agent-1"), cx) && store.timeline_settled_for("agent-1")
    });
    crate::test_frames::draw_settled(cx);
}

/// Press the control `selector` names, and let what it moves land.
fn press_control(selector: &'static str, cx: &mut VisualTestContext) {
    let control = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("no {selector}"));
    cx.simulate_mouse_move(control.center(), None, gpui::Modifiers::default());
    draw(cx);
    cx.simulate_click(control.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
}

/// Choose the palette's action `label`, in whatever language the host's
/// settings left the window in.
fn choose_from_palette(shell: &Entity<AppShell>, label: &str, cx: &mut VisualTestContext) {
    shell
        .read_with(cx, |shell, _| shell.window_state())
        .update(cx, |state, cx| state.open_palette(cx));
    draw(cx);
    cx.simulate_input(&format!(">{label}"));
    draw(cx);
    cx.simulate_keystrokes("enter");
    draw(cx);
}

/// [`mount_panes`]' `thread-1` on the left and `thread-2`, answered by
/// `serve`, on the right, the keyboard on the left.
fn side_by_side(shell: &Entity<AppShell>, serve: impl FnOnce(), cx: &mut VisualTestContext) {
    let row = cx
        .debug_bounds("sidebar-thread-thread-2")
        .expect("the thread's row");
    let alt = gpui::Modifiers {
        alt: true,
        ..Default::default()
    };
    cx.simulate_mouse_move(row.center(), None, alt);
    draw(cx);
    cx.simulate_click(row.center(), alt);
    crate::test_frames::draw_settled(cx);
    serve();
    await_restore_update(shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    crate::test_frames::draw_settled(cx);
    cx.simulate_keystrokes("secondary-alt-left");
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    assert_eq!(keyboard_pane(shell, cx), Some(0));
}

/// Press and release at `at`, a frame between the two as on screen.
fn press_with_a_frame_between(at: gpui::Point<Pixels>, cx: &mut VisualTestContext) {
    cx.simulate_mouse_move(at, None, gpui::Modifiers::default());
    draw(cx);
    cx.simulate_mouse_down(at, MouseButton::Left, gpui::Modifiers::default());
    draw(cx);
    cx.simulate_mouse_up(at, MouseButton::Left, gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
}

/// The panel `session_id`'s conversation has recorded, if it is open.
fn panel_of(
    store: &Entity<WorkspaceStore>,
    session_id: &str,
    cx: &VisualTestContext,
) -> Option<RightTab> {
    store.read_with(cx, |store, _| {
        let panel = store.panel_state_for(Some(session_id));
        panel.right_panel_open.then_some(panel.right_tab)
    })
}

/// Press `at` and let go elsewhere, as a pointer that changes its mind:
/// a press with no click.
fn press_only(at: gpui::Point<Pixels>, cx: &mut VisualTestContext) {
    let none = gpui::Modifiers::default();
    cx.simulate_mouse_down(at, MouseButton::Left, none);
    draw(cx);
    cx.simulate_mouse_up(gpui::point(px(1.), px(1.)), MouseButton::Left, none);
    draw(cx);
}

/// How far the selected fill has come on `row`.
fn row_fill(row: &'static str, cx: &mut VisualTestContext) -> f32 {
    let row = cx.debug_bounds(row).expect("the thread's row");
    cx.update(|window, cx| {
        let active = cx.theme().list_active;
        let row = row.scale(window.scale_factor());
        window
            .painted_quads()
            .iter()
            .filter(|quad| quad.bounds == row)
            .filter_map(|quad| quad.background.as_solid())
            .filter(|fill| (fill.h, fill.s, fill.l) == (active.h, active.s, active.l))
            .map(|fill| fill.a)
            .fold(0., f32::max)
    })
}

/// The first log of `id`: a line of text, in `markdown-block-0`.
fn serve_answer(host: &MountedShell, id: &str) {
    let records: Vec<agent::AgentEvent> = vec![
        agent::AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: "answer".into(),
            parent_item_id: None,
            content: agent::ItemContent::AssistantMessage {
                text: "Selectable answer alpha beta gamma".into(),
                summarized: false,
            },
        }),
        agent::AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: agent::TurnStatus::Completed,
            usage: None,
        },
    ];
    send_events(
        host,
        vec![(
            Topic::SessionEvents {
                session_id: id.into(),
            },
            ServerEvent::SessionSnapshot {
                total: 0,
                total_turns: 0,
                truncated: false,
                from: 0,
                end: records.len() as u64,
                records: records.into_iter().map(Into::into).collect(),
            },
        )],
    );
}

fn new_thread_from_projects(
    cx: &mut TestAppContext,
    mobile: bool,
    compact: bool,
    project_count: usize,
) {
    let (shell, host, cx) = mount(cx);
    cx.update(|_, cx| {
        if mobile {
            crate::window_seam::force_mobile_layout(cx);
        }
        cx.set_reduce_motion(true);
    });
    // Establish the viewport before the workspace can select a draft:
    // shrinking a populated wide layout opens its thread.
    cx.simulate_resize(size(px(if compact { 393. } else { 1024. }), px(852.)));
    crate::test_frames::draw_settled(cx);
    let projects: Vec<_> = (0..project_count)
        .map(|index| tcode_core::project::Project {
            id: format!("project-{index}"),
            name: format!("Project {index}"),
            root: PANES_CWD.into(),
            quick_chat: false,
            icon_path: None,
            permission_defaults: Default::default(),
            created_at: index as u64,
        })
        .collect();
    let chosen = projects.last().unwrap().id.clone();
    send_events(
        &host,
        vec![
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: Vec::new(),
                    projects,
                }),
            ),
        ],
    );
    await_restore_update(&shell, cx, |store, _| store.index_hydrated());
    draw(cx);
    let ack_start_draft = |project: Option<&str>, draft: &str| {
        let request = std::iter::from_fn(|| host.outgoing.try_recv().ok())
            .map(|line| decode_client_line(&line).expect("client line"))
            .find_map(|message| match message.payload {
                ClientPayload::Command(Command::StartDraft { project_id, .. }) => {
                    Some((message.id, project_id))
                }
                _ => None,
            })
            .expect("a draft was asked for");
        if let Some(project) = project {
            assert_eq!(request.1, project);
        }
        host.incoming
            .try_send(
                encode_line(&HostMessage::Ack {
                    id: request.0,
                    result: Ok(tcode_protocol::CommandResponse::SessionId(Some(
                        draft.into(),
                    ))),
                })
                .unwrap(),
            )
            .unwrap();
    };
    // The empty workspace's own draft, which nobody asked for.
    ack_start_draft(None, "fallback");
    await_restore_update(&shell, cx, |store, _| store.is_thread_open("fallback"));
    crate::test_frames::draw_settled(cx);

    for reopen in [false, true] {
        if reopen && compact {
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
            });
            crate::test_frames::draw_settled(cx);
        }
        if compact {
            let plus = cx
                .debug_bounds("compact-new-thread")
                .expect("Threads new-thread button");
            cx.simulate_click(plus.center(), gpui::Modifiers::default());
        } else {
            shell
                .read_with(cx, |shell, _| shell.window_state())
                .update(cx, |state, cx| state.open_palette(cx));
        }
        crate::test_frames::draw_settled(cx);
        if project_count > 1 {
            cx.update(|window, cx| {
                let palette = &shell.read(cx).palette;
                // Its query holds the keyboard where the palette
                // itself does not.
                assert_eq!(
                    !palette.read(cx).focus_handle(cx).is_focused(window),
                    !(compact && mobile),
                    "the chooser raises the keyboard only off a phone"
                );
                assert!(
                    palette
                        .read(cx)
                        .focus_handle(cx)
                        .contains_focused(window, cx),
                    "the palette owns focus so Escape closes it"
                );
            });
            let row = cx
                .debug_bounds("palette-new-thread-project-1")
                .expect("project choice");
            cx.simulate_click(row.center(), gpui::Modifiers::default());
        }
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert!(
                !shell.window_state.read(cx).palette_open,
                "choosing a project closes the chooser"
            );
        });

        ack_start_draft(Some(&chosen), "draft");
        await_restore_update(&shell, cx, |store, _| store.is_thread_open("draft"));
        serve_thread(&host, "draft");
        serve_status(&host, "draft", true);
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("draft"), cx)
        });
        crate::test_frames::draw_settled(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(
                shell
                    .page_thread(cx)
                    .map(|thread| thread.session_id)
                    .as_deref(),
                Some("draft")
            );
            let state = shell.window_state.read(cx);
            if compact {
                assert_eq!(
                    state.history().last(),
                    Some(&Destination::Thread),
                    "the host's answer navigates to the draft, reused or new"
                );
            } else {
                assert_eq!(state.route(), Route::Chat);
            }
        });

        let typed_into: &[&str] = if mobile { &[] } else { &["draft"] };
        assert_eq!(
            typed_reaches(&host, cx),
            typed_into,
            "the draft raises the keyboard only off a phone"
        );
        if mobile {
            let card = cx.debug_bounds("composer-card").expect("draft composer");
            cx.simulate_click(
                gpui::point(card.center().x, card.top() + px(24.)),
                gpui::Modifiers::default(),
            );
            draw(cx);
            assert_eq!(
                typed_reaches(&host, cx),
                ["draft"],
                "an explicit tap focuses"
            );
            let rotation = if compact {
                [1024., 393.]
            } else {
                [393., 1024.]
            };
            for width in rotation {
                cx.simulate_resize(size(px(width), px(852.)));
                crate::test_frames::draw_settled(cx);
                assert_eq!(
                    typed_reaches(&host, cx),
                    ["draft"],
                    "rotation keeps editing focus"
                );
            }
        }
    }
}

/// A shell of `client` on a real host over `disk`, showing nothing yet.
fn mount_on_host(
    cx: &mut TestAppContext,
    disk: tcode_services::store::SessionStore,
    client: Option<Rc<dyn ClientHost>>,
) -> (
    Entity<AppShell>,
    Entity<WorkspaceStore>,
    tcode_runtime::pipe::SpawnedHost,
    crate::store::tests::LocalHostDriver,
    &mut VisualTestContext,
) {
    let host = tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
        .expect("spawn test host");
    let driver = crate::store::tests::LocalHostDriver::new(&host);
    let (shell, _transport, cx) = mount_with(cx, Some(AttachmentTarget::Local), client.clone());
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| {
        *store = WorkspaceStore::new_attached(
            driver.link.clone(),
            crate::store::WorkspaceAttachment::Local,
            client,
            None,
            cx,
        );
    });
    (shell, store, host, driver, cx)
}

/// A real shell behind a threads list tested on its own: what `store`
/// shows lands in the pane the list reads the current pane from.
pub(crate) fn shell_behind(store: &Entity<WorkspaceStore>, cx: &mut TestAppContext) {
    let (shell, host, cx) = mount(cx);
    cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
    let own = store_of(&shell, cx);
    cx.update(|_, cx| {
        cx.subscribe(store, move |_, shown: &ShowThread, cx| {
            // Held here, the shell's link stays open as long as the store.
            let _ = &host;
            own.update(cx, |own, cx| own.show_thread(shown.session_id.clone(), cx));
        })
        .detach();
    });
}

fn mount(cx: &mut TestAppContext) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
    mount_initial(cx, Some(AttachmentTarget::Local))
}

fn mount_initial(
    cx: &mut TestAppContext,
    initial: Option<AttachmentTarget>,
) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
    mount_with(cx, initial, None)
}

fn mount_with(
    cx: &mut TestAppContext,
    initial: Option<AttachmentTarget>,
    client_host: Option<Rc<dyn ClientHost>>,
) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
    // As `run_shell` does before opening the window: the shell alone,
    // without the window root, initializes nothing of gpui-base itself.
    cx.update(crate::test_client::init);
    if let Some(client) = client_host {
        cx.update(|cx| crate::test_client::attach(client, true, cx));
    }
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let (states, state) = async_channel::unbounded();
    let transport = RefCell::new(Some(Transport {
        originals: None,
        to_host: to_host.into(),
        from_host,
        state,
        current_host: None,
    }));
    let mounted = Rc::new(RefCell::new(None));
    let capture = mounted.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let window_state = cx.new(|_| WindowState::default());
        let shell = cx.new(|cx| {
            AppShell::new(
                window_state,
                ShellSetup {
                    local: Some(Rc::new(move |_| {
                        Ok(transport.borrow_mut().take().expect("one attachment"))
                    })),
                    initial,
                    ..Default::default()
                },
                window,
                cx,
            )
        });
        *capture.borrow_mut() = Some(shell.clone());
        gpui_base::Root::new(shell, window, cx)
    });
    let shell = mounted.borrow_mut().take().unwrap();
    if let Some(store) = shell.read_with(cx, |shell, cx| shell.store(cx)) {
        crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
    }
    (
        shell,
        MountedShell {
            outgoing,
            observed: RefCell::new(Vec::new()),
            incoming,
            states,
        },
        cx,
    )
}

fn draw(cx: &mut VisualTestContext) {
    frame(cx, |_, _| ());
}

/// Draw a frame and `read` it as the screen shows it until the next one:
/// before the work it deferred runs, after which the test platform draws
/// again at once.
fn frame<R>(cx: &mut VisualTestContext, read: impl FnOnce(&mut Window, &mut App) -> R) -> R {
    cx.run_until_parked();
    cx.update(|window, cx| {
        // A frame that reuses any view keeps every debug bound of the
        // previous one, so an element no longer painted would still be found.
        window.refresh();
        _ = window.draw(cx);
        read(window, cx)
    })
}

fn resize(cx: &mut VisualTestContext, width: f32) {
    cx.simulate_resize(size(px(width), px(800.)));
    draw(cx);
}

/// Make the test app a mobile build: only there is a phone-sized window
/// compact, a desktop window being wide at any width.
fn as_mobile(cx: &mut VisualTestContext) {
    cx.update(|_, cx| crate::window_seam::force_mobile_layout(cx));
}

fn touch_drag(cx: &mut VisualTestContext, id: u64, steps: &[(gpui::TouchPhase, f32, f32, u64)]) {
    for &(phase, x, y, elapsed) in steps {
        cx.executor().advance_clock(Duration::from_millis(elapsed));
        cx.update(|window, cx| {
            window.dispatch_event(
                gpui::PlatformInput::Touch(gpui::TouchEvent {
                    id: gpui::TouchId(id),
                    phase,
                    position: gpui::point(px(x), px(y)),
                    predicted_position: None,
                    force: None,
                }),
                cx,
            );
        });
        draw(cx);
    }
}

/// Every line the client has sent since the last drain.
fn sent(host: &MountedShell) -> Vec<ClientPayload> {
    let mut payloads: Vec<_> = host
        .observed
        .borrow_mut()
        .drain(..)
        .map(|message| message.payload)
        .collect();
    while let Ok(line) = host.outgoing.try_recv() {
        payloads.push(decode_client_line(&line).expect("client line").payload);
    }
    payloads
}

fn store_of(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Entity<WorkspaceStore> {
    shell.read_with(cx, |shell, cx| shell.store(cx).expect("attached"))
}

/// A session the diff panel can address: the host replaces this value
/// wholesale, so the fields the panel does not read stay at their zero.
fn session_status(session: &str, cwd: &std::path::Path) -> tcode_protocol::SessionStatus {
    tcode_protocol::SessionStatus {
        session_id: session.into(),
        title: "Compact panel".into(),
        cwd: cwd.to_path_buf(),
        attachments_dir: cwd.to_path_buf(),
        provider: agent::ProviderKind::ClaudeCode,
        requested_model: None,
        requested_profile_id: None,
        acp_agent_id: None,
        project_id: None,
        queued_messages: Vec::new(),
        background_tasks: Vec::new(),
        review_comment_drafts: Vec::new(),
        terminals: Vec::new(),
        active_terminal_id: None,
        terminal_splits: Vec::new(),
        terminal_contexts: Vec::new(),
        terminal_open: false,
        terminal_height: 240.,
        delivery_in_flight: None,
        activity: tcode_protocol::SessionActivity {
            working: false,
            turn_running: false,
            background: Default::default(),
            unsettled: 0,
            wakes: Default::default(),
            waiting_for_approval: false,
            waiting_for_input: false,
            settle_blocked: false,
            failed: false,
            unread: false,
            last_activity_at: None,
            fork: tcode_protocol::ForkAvailability::Available,
            agent: None,
            latest_activity: None,
        },
        preparing_turn: false,
        stopping: false,
        stopping_since: None,
        turn_retry: None,
        goal: None,
        goal_control: None,
        manual_compaction: false,
        background_control: None,
        harness_option_labels: false,
        rewind: None,
        fork_at: None,
        queue_held: false,
        checkout_blocked: false,
        conversation_read_only: false,
        terminal_limit_reached: false,
        terminal_split_available: false,
        usage: None,
        context_window: None,
        running_turn: None,
        pending_approvals: Vec::new(),
        pending_user_input: None,
        steering_supported: false,
        // As the host describes every thread of a harness with one.
        provider_option_descriptors: agent::permission_control(agent::ProviderKind::ClaudeCode)
            .into_iter()
            .collect(),
        provider_option_selections: Vec::new(),
        provider_option_unconfirmed: Vec::new(),
        provider_option_requested_selections: Vec::new(),
        running_model: None,
        model_unconfirmed: false,
        provider_commands: Vec::new(),
        git_branch: None,
        branches: Vec::new(),
        draft: false,
        draft_workspace: Default::default(),
        worktree: None,
        preparing_worktree: false,
        relay_confirmation: None,
        model_pending_restart: false,
        options_pending_restart: false,
        injected_tools: None,
        control: None,
    }
}

/// Give the shell one selected session whose only net change is a single
/// unbreakable 400-character line, so the diff panel has real content far
/// wider than a phone page.
fn seed_wide_diff(shell: &Entity<AppShell>, host: &MountedShell, cx: &mut VisualTestContext) {
    seed_wide_diff_with(shell, host, cx, Vec::new());
}

/// [`seed_wide_diff`], with `extra` events after the turn's changes.
fn seed_wide_diff_with(
    shell: &Entity<AppShell>,
    host: &MountedShell,
    cx: &mut VisualTestContext,
    extra: Vec<agent::AgentEvent>,
) {
    let session = "thread-1";
    let cwd = std::path::Path::new("/tmp/tcode-compact-panel");
    let path = "a/deeply/nested/module/tree/with/an/absurdly/long/file_name.rs";
    let long = "x".repeat(400);
    let diff = format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n\
             @@ -1 +1 @@\n-fn old() {{}}\n+fn new_{long}() {{}}\n"
    );
    let events = [
        (
            Topic::SessionStatus {
                session_id: session.into(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(session_status(session, cwd))),
        ),
        (
            Topic::SessionEvents {
                session_id: session.into(),
            },
            ServerEvent::SessionSnapshot {
                total: 0,
                total_turns: 0,
                truncated: false,
                from: 0,
                end: 2 + extra.len() as u64,
                records: [
                    agent::AgentEvent::TurnStarted {
                        turn_id: "turn-1".into(),
                    },
                    agent::AgentEvent::TurnChangesUpdated {
                        turn_id: "turn-1".into(),
                        changes: agent::file_changes_from_unified_diff(&diff).changes,
                        completeness: agent::ChangeCompleteness::Exact,
                    },
                ]
                .into_iter()
                .chain(extra)
                .map(Into::into)
                .collect(),
            },
        ),
    ];
    for (topic, event) in events {
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }
    let store = store_of(shell, cx);
    store.update(cx, |store, cx| store.show_thread(session.into(), cx));
    cx.run_until_parked();
    cx.run_until_parked();
}

/// The panel first asks the host for each changed file's current text, then
/// builds its rendered files off the render thread. This workspace has no
/// checkout, so refuse those reads — the panel then renders from the stored
/// patch alone — and draw until `selector` is actually laid out. A
/// working-tree read is answered with `git_diff` when given; returns how
/// many the client made.
fn draw_until(
    cx: &mut VisualTestContext,
    host: &MountedShell,
    selector: &'static str,
    git_diff: Option<&tcode_protocol::GitDiffResult>,
) -> usize {
    let mut git_diff_reads = 0;
    for _ in 0..12 {
        cx.run_until_parked();
        while let Ok(line) = host.outgoing.try_recv() {
            let line = decode_client_line(&line).expect("client line");
            let ClientPayload::Query(query) = line.payload else {
                continue;
            };
            let result = match (&query, git_diff) {
                (tcode_protocol::Query::LoadGitDiff { .. }, Some(diff)) => {
                    git_diff_reads += 1;
                    Ok(tcode_protocol::QueryResponse::GitDiff(diff.clone()))
                }
                _ => Err(tcode_protocol::ProtocolError {
                    code: "not_found".into(),
                    message: "no checkout in this test".into(),
                }),
            };
            host.incoming
                .try_send(
                    encode_line(&HostMessage::QueryResult {
                        id: line.id,
                        result,
                    })
                    .unwrap(),
                )
                .unwrap();
        }
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
        draw(cx);
        if cx.debug_bounds(selector).is_some() {
            return git_diff_reads;
        }
    }
    panic!("{selector} never laid out");
}

/// An agent's thread with every right-panel tab, ten running agents of its
/// own, a two-digit count on Pull requests, the longest git label, all five
/// header toggles and a link back to a parent whose title is longer than
/// the link shows.
fn mount_crowded_thread(
    cx: &mut TestAppContext,
) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
    mount_thread_with(cx, ten_agents(agent::ItemStatus::InProgress))
}

/// Ten agents of the thread's own, each at `status`.
fn ten_agents(status: agent::ItemStatus) -> Vec<agent::AgentEvent> {
    (0..10)
        .map(|index| {
            agent::AgentEvent::ItemStarted(agent::ThreadItem {
                id: format!("agent-{index}"),
                parent_item_id: None,
                content: agent::ItemContent::Subagent {
                    agent_type: "explore".into(),
                    description: format!("Survey module {index}"),
                    status,
                    summary: None,
                    model: None,
                    effort: None,
                },
            })
        })
        .collect()
}

/// [`mount_crowded_thread`]'s thread with `events` in its log in place of
/// its running agents, which show they run for as long as they do.
fn mount_thread_with(
    cx: &mut TestAppContext,
    events: Vec<agent::AgentEvent>,
) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
    let (shell, host, cx) = mount(cx);
    let mut meta = tcode_core::project::SessionMeta::new(
        agent::ProviderKind::ClaudeCode,
        "/tmp/tcode-compact-panel".into(),
        None,
    );
    let mut parent = tcode_core::project::SessionMeta::new(
        agent::ProviderKind::ClaudeCode,
        "/tmp/tcode-compact-panel".into(),
        None,
    );
    parent.id = "thread-0".into();
    parent.title = "Plan the checkout service retry and timeout policy".into();
    meta.id = "thread-1".into();
    meta.parent_session_id = Some(parent.id.clone());
    meta.pull_requests = (1..=10)
        .map(|number| tcode_core::pull_request::ThreadPullRequestLink {
            key: tcode_core::pull_request::PullRequestKey {
                host: "github.com".into(),
                repository: "acme/api".into(),
                number,
            },
            source: tcode_core::pull_request::PullRequestSource::Manual,
            url: String::new(),
            linked_at: None,
            snapshot: None,
            stack: Default::default(),
            sync_error: None,
            watch: None,
        })
        .collect();
    let git = tcode_core::git::GitStatus {
        is_repo: true,
        has_commits: true,
        has_upstream: true,
        has_working_tree_changes: true,
        ..Default::default()
    };
    for (topic, event) in [
        (
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        ),
        (
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions: vec![parent, meta],
                projects: Vec::new(),
            }),
        ),
        (
            Topic::GitStatus {
                session_id: "thread-1".into(),
            },
            ServerEvent::GitStatusReplaced(tcode_protocol::GitStatusStatus {
                status: Some(git),
                error: None,
                busy: false,
            }),
        ),
    ] {
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }
    seed_wide_diff_with(&shell, &host, cx, events);
    (shell, host, cx)
}

/// The row of the conversation at the top of the timeline: the first
/// one drawn that reaches into it.
fn top_row(cx: &mut VisualTestContext) -> Option<usize> {
    let top = cx.debug_bounds("chat-timeline")?.top();
    (0..1_000).find(|row| {
        cx.debug_bounds(format!("timeline-row-{row}").leak())
            .is_some_and(|bounds| bounds.bottom() > top)
    })
}

/// A thread whose turn is running, with the diff panel open on its working tree.
fn open_working_tree_panel(
    shell: &Entity<AppShell>,
    host: &MountedShell,
    cx: &mut VisualTestContext,
) {
    let session = "thread-1";
    let cwd = std::path::Path::new("/tmp/tcode-working-tree");
    let events = [
        (
            Topic::SessionStatus {
                session_id: session.into(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(session_status(session, cwd))),
        ),
        (
            Topic::SessionEvents {
                session_id: session.into(),
            },
            ServerEvent::SessionSnapshot {
                total: 0,
                total_turns: 0,
                truncated: false,
                from: 0,
                end: 1,
                records: vec![
                    agent::AgentEvent::TurnStarted {
                        turn_id: "turn-1".into(),
                    }
                    .into(),
                ],
            },
        ),
    ];
    for (topic, event) in events {
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }
    let store = store_of(shell, cx);
    store.update(cx, |store, cx| store.show_thread(session.into(), cx));
    cx.run_until_parked();
    cx.run_until_parked();
    resize(cx, 1200.);
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for(session, tcode_core::ui::RightTab::Diff, cx)
    });
}

/// Lets the client run and draw while the host holds every working-tree read; any other
/// query is refused, as a workspace without a checkout would. Returns the held reads in
/// the order the client made them.
fn held_git_diff_reads(host: &MountedShell, cx: &mut VisualTestContext) -> Vec<u64> {
    let mut reads = Vec::new();
    for _ in 0..6 {
        cx.run_until_parked();
        while let Ok(line) = host.outgoing.try_recv() {
            let line = decode_client_line(&line).expect("client line");
            let ClientPayload::Query(query) = line.payload else {
                continue;
            };
            if matches!(query, tcode_protocol::Query::LoadGitDiff { .. }) {
                reads.push(line.id);
                continue;
            }
            answer_query(
                host,
                line.id,
                Err(tcode_protocol::ProtocolError {
                    code: "not_found".into(),
                    message: "no checkout in this test".into(),
                }),
            );
        }
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
        draw(cx);
    }
    reads
}

fn answer_query(
    host: &MountedShell,
    id: u64,
    result: Result<tcode_protocol::QueryResponse, tcode_protocol::ProtocolError>,
) {
    host.incoming
        .try_send(encode_line(&HostMessage::QueryResult { id, result }).unwrap())
        .unwrap();
}

fn answer_git_diff(host: &MountedShell, id: u64, diff: &tcode_protocol::GitDiffResult) {
    answer_query(
        host,
        id,
        Ok(tcode_protocol::QueryResponse::GitDiff(diff.clone())),
    );
}

/// A working tree in which each of `files` was created with `lines` lines.
fn created_files(files: &[(&str, u32)]) -> tcode_protocol::GitDiffResult {
    let (changes, texts) = files
        .iter()
        .map(|&(path, lines)| {
            let text = (1..=lines).map(|line| format!("{path} line {line}\n"));
            let patch = format!(
                "@@ -0,0 +1,{lines} @@\n{}",
                text.clone()
                    .map(|line| format!("+{line}"))
                    .collect::<String>()
            );
            (
                agent::FileChange {
                    path: path.into(),
                    kind: agent::FileChangeKind::Create,
                    diff: Some(patch),
                },
                tcode_protocol::GitFileText {
                    old: None,
                    new: Some(text.collect()),
                },
            )
        })
        .unzip();
    tcode_protocol::GitDiffResult {
        changes,
        texts,
        ..Default::default()
    }
}

/// The agent ends a turn, so the working tree is read again.
fn complete_turn(host: &MountedShell) {
    host.incoming
        .try_send(
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: "thread-1".into(),
                },
                event: ServerEvent::SessionEvent(
                    agent::AgentEvent::TurnCompleted {
                        turn_id: "turn-1".into(),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    }
                    .into(),
                ),
            }))
            .unwrap(),
        )
        .unwrap();
}

/// The host serves thread-1 as a running turn holding a question, a
/// first answer, and the command it runs now, which has printed two
/// short lines so far: none long enough to wrap at any width the tests
/// draw, so only motion changes their heights.
fn serve_running_command(host: &MountedShell) {
    let item = |id: &str, content| agent::ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content,
    };
    let mut status = session_status("thread-1", std::path::Path::new(PANES_CWD));
    status.activity.working = true;
    status.activity.turn_running = true;
    status.running_turn = Some(tcode_core::session::RunningTurn {
        turn: 0,
        started_at: None,
    });
    scenes::serve_log(
        host,
        status,
        vec![
            scenes::started(),
            agent::AgentEvent::ItemCompleted(item(
                "user",
                agent::ItemContent::UserMessage {
                    text: "Look at the build.".into(),
                    attachments: Vec::new(),
                },
            )),
            agent::AgentEvent::ItemCompleted(item(
                "reply",
                agent::ItemContent::AssistantMessage {
                    text: "I will build it first.".into(),
                    summarized: false,
                },
            )),
            agent::AgentEvent::ItemStarted(item(
                "cmd",
                agent::ItemContent::CommandExecution {
                    command: "cargo build".into(),
                    input: Vec::new(),
                    output: "line 0\nline 1\n".into(),
                    exit_code: None,
                    status: agent::ItemStatus::InProgress,
                },
            )),
        ],
    );
}

fn click_command(cx: &mut VisualTestContext) {
    let header = cx
        .debug_bounds("activity-row-cmd")
        .expect("the command's header");
    cx.simulate_click(header.center(), gpui::Modifiers::default());
}

mod drafts;
mod hosts;
mod keyboard;
mod launch;
mod navigation;
mod panes;
mod right_panel;
