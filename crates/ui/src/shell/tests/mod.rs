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

/// A machine refusing this device, and pairing with it again, change
/// nothing on another machine of the window: its store, pane, thread
/// and half-written message stay, and it is sent nothing. The refused
/// machine says so, and offers Pair again, in its own section.
#[gpui::test]
fn a_host_refused_and_paired_again_changes_nothing_on_another(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, hosts, client, cx) = mount_workspace(cx, two_hosts(), Default::default(), 1200.);
    cx.update(|_, cx| {
        crate::shortcut::init(cx);
        DevicePreferences::set_sidebar_layout(tcode_core::settings::SidebarLayout::Grouped, cx)
    });
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    assert_eq!(linked(&shell, cx), [key_a.clone(), key_b.clone()]);
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    hosts[&key_a].come_up(&a, Vec::new(), None, cx);
    hosts[&key_b].come_up(&b, Vec::new(), None, cx);
    send_events(&hosts[&key_a].host, vec![panes_index(&[])]);
    serve_thread(&hosts[&key_a].host, "thread-1");
    a.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    let card = cx.debug_bounds("composer-card").expect("A's composer");
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    cx.simulate_input("half a thought");
    draw(cx);
    let pane = shell.read_with(cx, |shell, _| {
        shell
            .column
            .as_ref()
            .expect("a column")
            .current_pane
            .clone()
    });
    let link_a = a.read_with(cx, |store, _| store.connection_state());
    sent(&hosts[&key_a].host);
    let unchanged = |cx: &mut VisualTestContext| {
        assert_eq!(store_of(&shell, cx), a, "the column stays on A");
        shell.read_with(cx, |shell, cx| {
            let column = shell.column.as_ref().unwrap();
            assert_eq!(column.current_pane, pane);
            assert_eq!(column.panes(cx), std::slice::from_ref(&pane));
        });
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.session_id().map(str::to_owned)),
            Some("thread-1".into())
        );
        a.read_with(cx, |store, cx| {
            assert!(store.is_thread_open("thread-1"));
            assert!(!store.chat_loading_for(Some("thread-1"), cx));
            assert_eq!(store.connection_state(), link_a);
        });
        assert_eq!(sent(&hosts[&key_a].host), Vec::new(), "A hears nothing");
    };

    hosts[&key_b]
        .host
        .states
        .try_send(tcode_client::ConnectionState::Offline {
            reason: tcode_client::ConnectionFailure::AuthenticationRejected,
        })
        .unwrap();
    crate::test_frames::draw_settled(cx);
    let again = cx
        .debug_bounds("host-pair-again-host-b")
        .expect("B offers Pair again in its section");
    unchanged(cx);

    let paired_again = WorkspaceHost::offer(&client, "host-b", cx);
    cx.simulate_click(again.center(), gpui::Modifiers::default());
    pair_with(&shell, &client, saved_machine("host-b", true), cx);
    assert_eq!(linked(&shell, cx), [key_a.clone(), key_b.clone()]);
    let b = store_on(&shell, "host-b", cx);
    paired_again.come_up(&b, Vec::new(), None, cx);
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("host-pair-again-host-b").is_none());
    assert!(b.read_with(cx, |store, _| store.connection_state().is_connected()));
    unchanged(cx);
    let card = cx.debug_bounds("composer-card").expect("A's composer");
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(
        turns_sent(&hosts[&key_a].host),
        [("thread-1".to_owned(), "half a thought".to_owned())],
        "the half-written message is still A's"
    );
}

/// Leaving the app parks every host's link, and coming back reconnects
/// every one.
#[gpui::test]
fn park_and_reconnect_reach_every_host(cx: &mut TestAppContext) {
    use tcode_client::recovery::Wake;
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, _client, cx) = mount_workspace(cx, two_hosts(), Default::default(), 393.);
    for wake in [Wake::Park, Wake::Reconnect] {
        shell.read_with(cx, |shell, _| shell.wake_connection(wake.clone()));
        for host in ["host-a", "host-b"] {
            assert_eq!(
                hosts[&HostKey::Remote(host.into())].wakes.try_recv(),
                Ok(wake.clone()),
                "{host}"
            );
        }
    }
}

/// Only the column's host is asked for its last project's draft; once
/// that host leaves the workspace, the column goes to the host left,
/// which is asked for its own, and the host left stays out.
#[gpui::test]
fn only_the_columns_host_offers_its_draft_until_it_leaves(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, client, cx) = mount_workspace(cx, two_hosts(), Default::default(), 1200.);
    let project = |id: &str| crate::store::tests::project_at(id, std::path::Path::new(PANES_CWD));
    let drafted = |host: &WorkspaceHost| -> Vec<String> {
        sent(&host.host)
            .into_iter()
            .filter_map(|payload| match payload {
                ClientPayload::Command(Command::StartDraft { project_id, .. }) => Some(project_id),
                _ => None,
            })
            .collect()
    };
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    hosts[&key_a].come_up(&a, vec![project("on-a")], Some("on-a"), cx);
    hosts[&key_b].come_up(&b, vec![project("on-b")], Some("on-b"), cx);
    assert_eq!(drafted(&hosts[&key_a]), ["on-a"]);
    assert_eq!(drafted(&hosts[&key_b]), Vec::<String>::new());

    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.leave_host(&key_a, window, cx)));
    draw(cx);
    assert_eq!(linked(&shell, cx), std::slice::from_ref(&key_b));
    assert_eq!(store_of(&shell, cx), b);
    assert_eq!(drafted(&hosts[&key_b]), ["on-b"]);
    let workspace = client
        .load_preferences()
        .unwrap()
        .workspace
        .expect("arranged");
    assert_eq!(workspace.left, ["host-a"]);
    assert!(workspace.hosts.iter().all(|host| host.host_id != "host-a"));
}

/// A window reopens on the thread it showed and on that thread's host:
/// the column starts there though another host comes first, with the
/// thread in its pane before the baseline, so no host is asked for a
/// draft. The other host is opened and listened to, nothing more.
#[gpui::test]
fn a_cold_start_reopens_the_thread_on_the_host_it_showed(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, _client, cx) = mount_workspace(
        cx,
        two_hosts(),
        tcode_client::host::ClientPreferences {
            navigation: Some(serde_json::json!({
                "history": ["threads", "thread"],
                "thread": { "host": "host-b", "session_id": "thread-2" },
                "panel": "diff"
            })),
            ..Default::default()
        },
        393.,
    );
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let thread = ThreadKey {
        host: key_b.clone(),
        session_id: "thread-2".into(),
    };
    assert_eq!(linked(&shell, cx), [key_a.clone(), key_b.clone()]);
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    assert_eq!(store_of(&shell, cx), b, "the column is on B");
    let restored = |cx: &mut VisualTestContext| {
        shell.read_with(cx, |shell, cx| {
            assert_eq!(
                shell.window_state.read(cx).history(),
                [Destination::Threads, Destination::Thread]
            );
            assert_eq!(shell.page_thread(cx), Some(thread.clone()));
        })
    };
    restored(cx);

    let project = |id: &str| crate::store::tests::project_at(id, std::path::Path::new(PANES_CWD));
    hosts[&key_a].come_up(&a, vec![project("on-a")], Some("on-a"), cx);
    let on_b = &hosts[&key_b].host;
    crate::store::tests::seed_full_scope(&b, &on_b.incoming, Vec::new(), cx);
    let (index, ServerEvent::IndexSnapshot(mut listed)) = panes_index(&[]) else {
        unreachable!("an index")
    };
    listed.projects = vec![project("on-b")];
    send_events(
        on_b,
        vec![
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                    last_project_id: Some("on-b".into()),
                    ..Default::default()
                }),
            ),
            (index, ServerEvent::IndexSnapshot(listed)),
        ],
    );
    serve_thread(on_b, "thread-2");
    on_b.states
        .try_send(tcode_client::ConnectionState::Connected { path: None })
        .unwrap();
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    restored(cx);

    let to_b = sent(on_b);
    assert_eq!(
            to_b.iter()
                .filter(|payload| matches!(payload,
                    ClientPayload::Subscribe(subscription) if subscription.topic == Topic::SessionEvents { session_id: "thread-2".into() }
                ))
                .count(),
            1,
            "B is asked for the thread once"
        );
    assert!(
        !to_b.iter().any(|payload| matches!(
            payload,
            ClientPayload::Command(Command::StartDraft { .. } | Command::StartScratchDraft)
        )),
        "B is asked for no draft"
    );
    let to_a = sent(&hosts[&key_a].host);
    for payload in &to_a {
        assert!(
            !matches!(payload, ClientPayload::Command(_)),
            "A is asked to do nothing: {payload:?}"
        );
    }
    assert!(
            !to_a.iter().any(|payload| matches!(payload,
                ClientPayload::Subscribe(subscription) if matches!(subscription.topic, Topic::SessionEvents { .. } | Topic::SessionStatus { .. })
            )),
            "A is asked for no thread"
        );
    assert!(a.read_with(cx, |store, _| store.open_threads().is_empty()));
}

/// Pairing with a machine adds it beside the window's hosts, leaving
/// their links as they are; pairing into a space replaces them, as a
/// space is reached alone.
#[gpui::test]
fn pairing_adds_a_machine_beside_the_others_and_a_space_replaces_them(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, client, cx) = mount_workspace(
        cx,
        vec![saved_machine("host-a", true)],
        Default::default(),
        1200.,
    );
    let key_a = HostKey::Remote("host-a".into());
    let a = store_on(&shell, "host-a", cx);
    hosts[&key_a].come_up(&a, Vec::new(), None, cx);

    let _c = WorkspaceHost::offer(&client, "host-c", cx);
    pair_with(&shell, &client, saved_machine("host-c", false), cx);
    assert_eq!(
        linked(&shell, cx),
        [key_a.clone(), HostKey::Remote("host-c".into())]
    );
    assert_eq!(store_on(&shell, "host-a", cx), a, "A's link is kept");
    assert!(a.read_with(cx, |store, _| store.connection_state().is_connected()));
    assert_eq!(store_of(&shell, cx), a, "the column stays");
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell.window_state.read(cx).route()),
        Route::Chat
    );

    let _d = WorkspaceHost::offer(&client, "host-d", cx);
    pair_with(
        &shell,
        &client,
        tcode_client::pairing::PairedHost {
            space_id: Some("space".into()),
            space_name: Some("Shared".into()),
            ..saved_machine("host-d", false)
        },
        cx,
    );
    assert_eq!(linked(&shell, cx), [HostKey::Remote("host-d".into())]);
}

/// A group this device comes to know of — the machine in this process
/// joined it, which writes its members beside the saved machines — opens
/// its members at once, beside the hosts already open: no machine has to
/// connect again first.
#[gpui::test]
fn a_groups_members_open_as_soon_as_the_device_knows_them(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, client, cx) = mount_workspace(
        cx,
        vec![saved_machine("host-a", false)],
        tcode_client::host::ClientPreferences {
            workspace: Some(crate::workspace::arranged(
                None,
                &[HostKey::Remote("host-a".into())],
            )),
            ..Default::default()
        },
        1200.,
    );
    let key_a = HostKey::Remote("host-a".into());
    let a = store_on(&shell, "host-a", cx);
    hosts[&key_a].come_up(&a, Vec::new(), None, cx);

    let _b = WorkspaceHost::offer(&client, "host-b", cx);
    client.remember_host(saved_machine("host-b", true)).unwrap();
    draw(cx);
    assert_eq!(
        linked(&shell, cx),
        [key_a.clone(), HostKey::Remote("host-b".into())]
    );
    assert_eq!(store_on(&shell, "host-a", cx), a, "A's link is kept");
    assert_eq!(store_of(&shell, cx), a, "the column stays");
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

/// A thread of host B opened beside a pane of host A's: both show, each
/// over its own host, each header naming its host once the panes span
/// two, and a message sent from each pane goes to that pane's host
/// alone. The thread of the same id on the other host stays closed.
#[gpui::test]
fn a_thread_of_another_host_opens_beside_and_each_pane_sends_to_its_own(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, hosts, cx) = a_thread_of_a_beside_b(cx, 1600.);
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    assert_eq!(named_hosts(&shell, cx), [None], "one host, no name");
    let on_b = b.read_with(cx, |store, _| store.thread_key("thread-1"));
    cx.update(|window, cx| window.dispatch_action(Box::new(OpenInNewPane(on_b)), cx));
    serve_thread(&hosts[&key_b].host, "thread-1");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-1".into())]
    );
    assert_eq!(pane_hosts(&shell, cx), [key_a.clone(), key_b.clone()]);
    assert_eq!(store_of(&shell, cx), b, "B's pane is the current one");
    let named = named_hosts(&shell, cx);
    assert!(
        named.iter().all(Option::is_some) && named[0] != named[1],
        "each header names its own host: {named:?}"
    );
    for store in [&a, &b] {
        assert!(store.read_with(cx, |store, _| store.is_thread_open("thread-1")));
        assert!(!store.read_with(cx, |store, _| store.is_thread_open("thread-2")));
    }
    let _ = sent(&hosts[&key_a].host);
    let _ = sent(&hosts[&key_b].host);

    let panes = shell.read_with(cx, |shell, cx| shell.column.as_ref().unwrap().panes(cx));
    for (pane, text) in panes.iter().zip(["to a", "to b"]) {
        type_into(pane, cx);
        cx.simulate_input(text);
        cx.simulate_keystrokes("enter");
        draw(cx);
    }
    assert_eq!(
        turns_sent(&hosts[&key_a].host),
        [("thread-1".to_owned(), "to a".to_owned())]
    );
    assert_eq!(
        turns_sent(&hosts[&key_b].host),
        [("thread-1".to_owned(), "to b".to_owned())]
    );
}

/// A relaunch returns to the host of the current pane, wherever the
/// window moved it: to a pane opened beside over another host, back to
/// a pane the keyboard enters, and to the host of a thread the current
/// pane comes to show.
#[gpui::test]
fn a_relaunch_returns_to_the_current_panes_host(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, hosts, cx) = a_thread_of_a_beside_b(cx, 1600.);
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let returns_to = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            let client = cx.global::<crate::remote::ClientAttachment>().host();
            crate::last_host_target(client.as_ref()).map(|target| target.key())
        })
    };
    assert_eq!(returns_to(cx), Some(key_a.clone()));

    let b = store_on(&shell, "host-b", cx);
    let on_b = b.read_with(cx, |store, _| store.thread_key("thread-1"));
    cx.update(|window, cx| window.dispatch_action(Box::new(OpenInNewPane(on_b)), cx));
    serve_thread(&hosts[&key_b].host, "thread-1");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_hosts(&shell, cx), [key_a.clone(), key_b.clone()]);
    assert_eq!(returns_to(cx), Some(key_b.clone()), "the pane beside");

    let panes = shell.read_with(cx, |shell, cx| shell.column.as_ref().unwrap().panes(cx));
    type_into(&panes[0], cx);
    assert_eq!(store_of(&shell, cx), store_on(&shell, "host-a", cx));
    assert_eq!(returns_to(cx), Some(key_a.clone()), "the pane entered");

    b.update(cx, |store, cx| store.show_thread("thread-2".into(), cx));
    serve_thread(&hosts[&key_b].host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_hosts(&shell, cx), [key_b.clone(), key_b.clone()]);
    assert_eq!(returns_to(cx), Some(key_b), "the thread the pane shows");
}

/// A row of host B's dropped on the edge of host A's pane opens beside
/// it, over B, and A's thread stays in its pane.
#[gpui::test]
fn a_row_of_another_host_dropped_on_a_panes_edge_opens_beside_it(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _hosts, cx) = a_thread_of_a_beside_b(cx, 1200.);
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    let pane = only_pane(&shell, cx);
    let to = gpui::point(pane.right() - px(40.), pane.center().y);
    drag_row("sidebar-thread-host-b/thread-2", to, cx);
    assert!(
        cx.debug_bounds("pane-drop-indicator").is_some(),
        "the drop would land"
    );
    cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    assert_eq!(
        pane_hosts(&shell, cx),
        [
            HostKey::Remote("host-a".into()),
            HostKey::Remote("host-b".into())
        ]
    );
    assert!(b.read_with(cx, |store, _| store.is_thread_open("thread-2")));
    assert!(a.read_with(cx, |store, _| store.is_thread_open("thread-1")));
    assert!(!a.read_with(cx, |store, _| store.is_thread_open("thread-2")));
}

/// Host B refusing the device leaves A's pane as it was — its thread,
/// its conversation with its scroll, its half-written message — and B's
/// pane showing what B's store holds. B leaving the workspace takes its
/// pane out and nothing else.
#[gpui::test]
fn another_host_refused_or_gone_leaves_a_panes_conversation(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, hosts, cx) = a_thread_of_a_beside_b(cx, 1600.);
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let on_b = b.read_with(cx, |store, _| store.thread_key("thread-2"));
    cx.update(|window, cx| window.dispatch_action(Box::new(OpenInNewPane(on_b)), cx));
    serve_thread(&hosts[&key_b].host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    crate::test_frames::draw_settled(cx);
    let [pane_a, pane_b]: [Entity<ThreadPanel>; 2] = shell
        .read_with(cx, |shell, cx| shell.column.as_ref().unwrap().panes(cx))
        .try_into()
        .expect("two panes");
    let chat_a = pane_a.read_with(cx, |pane, _| pane.chat());
    type_into(&pane_a, cx);
    cx.simulate_input("half a thought");
    draw(cx);
    let _ = sent(&hosts[&key_a].host);
    let a_unchanged = |cx: &mut VisualTestContext| {
        assert_eq!(
            pane_a.read_with(cx, |pane, cx| pane.thread(cx)),
            Some(a.read_with(cx, |store, _| store.thread_key("thread-1")))
        );
        assert_eq!(
            pane_a.read_with(cx, |pane, _| pane.chat()),
            chat_a,
            "the same conversation, scrolled where it was"
        );
        assert!(a.read_with(cx, |store, cx| store.is_thread_open("thread-1")
            && !store.chat_loading_for(Some("thread-1"), cx)));
        assert_eq!(sent(&hosts[&key_a].host), Vec::new(), "A hears nothing");
    };

    hosts[&key_b]
        .host
        .states
        .try_send(tcode_client::ConnectionState::Offline {
            reason: tcode_client::ConnectionFailure::AuthenticationRejected,
        })
        .unwrap();
    crate::test_frames::draw_settled(cx);
    a_unchanged(cx);
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell.column.as_ref().unwrap().panes(cx)),
        [pane_a.clone(), pane_b.clone()],
        "B's pane stays"
    );
    assert_eq!(
        pane_b.read_with(cx, |pane, cx| pane.thread(cx)),
        Some(b.read_with(cx, |store, _| store.thread_key("thread-2")))
    );

    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.leave_host(&key_b, window, cx)));
    crate::test_frames::draw_settled(cx);
    a_unchanged(cx);
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell.column.as_ref().unwrap().all_panes(cx)),
        std::slice::from_ref(&pane_a),
        "B's pane has gone"
    );
    assert_eq!(store_of(&shell, cx), a);
    type_into(&pane_a, cx);
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(
        turns_sent(&hosts[&key_a].host),
        [("thread-1".to_owned(), "half a thought".to_owned())],
        "the half-written message is still A's"
    );
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

/// Over two hosts the palette lists each host's threads under that
/// host's name, ranked within it. B's thread chosen there shows from B,
/// and B's command goes to B alone.
#[gpui::test]
fn the_palette_lists_each_hosts_threads_under_its_name_and_acts_on_that_host(
    cx: &mut TestAppContext,
) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, hosts, cx) = a_thread_of_a_beside_b(cx, 1200.);
    // The device took up the system's language.
    crate::set_locale(crate::LANGUAGE_ENGLISH);
    let a = store_on(&shell, "host-a", cx);
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    threads_in_a_project(&hosts, cx);
    search_palette(&shell, "thread-2", cx);
    for caption in ["Threads on host-a", "Threads on host-b"] {
        assert!(
            cx.debug_bounds(format!("palette-caption-{caption}").leak())
                .is_some(),
            "{caption}"
        );
    }
    assert!(cx.debug_bounds("palette-caption-Threads").is_none());
    // A's `thread-2`, then B's.
    click("palette-row-1", cx);
    serve_thread(&hosts[&key_b].host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_hosts(&shell, cx), std::slice::from_ref(&key_b));
    assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
    assert!(!a.read_with(cx, |store, _| store.is_thread_open("thread-2")));

    heard(&hosts[&key_a], cx);
    heard(&hosts[&key_b], cx);
    search_palette(&shell, "quick", cx);
    // A's quick thread, then B's.
    click("palette-row-1", cx);
    assert_eq!(drafts_asked(&hosts[&key_b]), [None]);
    assert_eq!(drafts_asked(&hosts[&key_a]), []);

    // A host that refuses the device offers nothing there: neither its
    // threads nor a thread started on it.
    hosts[&key_b]
        .host
        .states
        .try_send(tcode_client::ConnectionState::Offline {
            reason: tcode_client::ConnectionFailure::AuthenticationRejected,
        })
        .unwrap();
    crate::test_frames::draw_settled(cx);
    search_palette(&shell, "thread-2", cx);
    assert!(
        cx.debug_bounds("palette-caption-Threads on host-a")
            .is_some()
    );
    assert!(
        cx.debug_bounds("palette-caption-Threads on host-b")
            .is_none()
    );
    shell
        .read_with(cx, |shell, _| shell.window_state())
        .update(cx, |state, cx| state.close_palette(cx));
    crate::test_frames::draw_settled(cx);
    search_palette(&shell, "quick", cx);
    assert!(cx.debug_bounds("palette-row-1").is_none(), "A's alone");
    click("palette-row-0", cx);
    assert_eq!(drafts_asked(&hosts[&key_a]), [None]);
    assert_eq!(drafts_asked(&hosts[&key_b]), []);
}

/// A search of the messages asks every host, lists each host's hits
/// under its name, and a hit of B's opens on B.
#[gpui::test]
fn a_message_search_asks_every_host_and_opens_a_hit_on_its_own(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, cx) = a_thread_of_a_beside_b(cx, 1200.);
    // The device took up the system's language.
    crate::set_locale(crate::LANGUAGE_ENGLISH);
    let key_b = HostKey::Remote("host-b".into());
    search_palette(&shell, "zebra", cx);
    cx.executor().advance_clock(Duration::from_millis(200));
    draw(cx);
    for (host, session) in [("host-a", "thread-1"), ("host-b", "thread-2")] {
        let host = &hosts[&HostKey::Remote(host.into())].host;
        let asked = std::iter::from_fn(|| host.outgoing.try_recv().ok())
            .map(|line| decode_client_line(&line).expect("client line"))
            .filter(|line| {
                matches!(
                    &line.payload,
                    ClientPayload::Query(tcode_protocol::Query::SearchSessionContent {
                        query,
                        ..
                    }) if query == "zebra"
                )
            })
            .map(|line| line.id)
            .collect::<Vec<_>>();
        let [id] = asked[..] else {
            panic!("one search asked of each host: {asked:?}");
        };
        answer_query(
            host,
            id,
            Ok(tcode_protocol::QueryResponse::SessionContentHits(
                tcode_protocol::SessionContentHits {
                    hits: vec![tcode_protocol::SessionSearchHit {
                        session_id: session.into(),
                        session_title: session.into(),
                        entry_id: "entry-1".into(),
                        turn: 0,
                        snippet: "a zebra".into(),
                    }],
                    truncated: false,
                    unreadable: Vec::new(),
                },
            )),
        );
    }
    crate::test_frames::draw_settled(cx);
    for caption in ["Messages on host-a", "Messages on host-b"] {
        assert!(
            cx.debug_bounds(format!("palette-caption-{caption}").leak())
                .is_some(),
            "{caption}"
        );
    }
    // A's hit, then B's.
    click("palette-row-1", cx);
    serve_thread(&hosts[&key_b].host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_hosts(&shell, cx), [key_b]);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
}

/// A host's answer to a search the user has since typed past is not
/// listed, even when it arrives after the answer to what they typed.
#[gpui::test]
fn a_search_answer_for_an_earlier_query_is_not_listed(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, cx) = a_thread_of_a_beside_b(cx, 1200.);
    crate::set_locale(crate::LANGUAGE_ENGLISH);
    let host = &hosts[&HostKey::Remote("host-a".into())].host;
    let asked = |text: &str| {
        std::iter::from_fn(|| host.outgoing.try_recv().ok())
            .map(|line| decode_client_line(&line).expect("client line"))
            .filter(|line| {
                matches!(
                    &line.payload,
                    ClientPayload::Query(tcode_protocol::Query::SearchSessionContent {
                        query,
                        ..
                    }) if query == text
                )
            })
            .map(|line| line.id)
            .collect::<Vec<_>>()
    };
    let hits = |count: usize| {
        Ok(tcode_protocol::QueryResponse::SessionContentHits(
            tcode_protocol::SessionContentHits {
                hits: (0..count)
                    .map(|index| tcode_protocol::SessionSearchHit {
                        session_id: "thread-1".into(),
                        session_title: "thread-1".into(),
                        entry_id: format!("entry-{index}"),
                        turn: 0,
                        snippet: "a zebra".into(),
                    })
                    .collect(),
                unreadable: Vec::new(),
                truncated: false,
            },
        ))
    };
    search_palette(&shell, "zebra", cx);
    cx.executor().advance_clock(Duration::from_millis(200));
    draw(cx);
    let [earlier] = asked("zebra")[..] else {
        panic!("the first query is asked");
    };
    cx.simulate_input("s");
    cx.executor().advance_clock(Duration::from_millis(200));
    draw(cx);
    let [later] = asked("zebras")[..] else {
        panic!("the second query is asked");
    };

    answer_query(host, later, hits(1));
    answer_query(host, earlier, hits(2));
    crate::test_frames::draw_settled(cx);
    assert!(
        cx.debug_bounds("palette-caption-Messages on host-a")
            .is_some()
    );
    assert!(cx.debug_bounds("palette-row-0").is_some());
    assert!(
        cx.debug_bounds("palette-row-1").is_none(),
        "the earlier query's two hits are not listed"
    );
}

/// Over two hosts a host's toast names it and its action goes to it
/// alone, and an operation of one host is not taken for the other's
/// of the same number.
#[gpui::test]
fn a_hosts_toast_names_it_and_its_operations_stay_its_own(cx: &mut TestAppContext) {
    use tcode_protocol::{
        ProviderUpdateAvailable, ProviderUpdateRun, RuntimeNotification, RuntimeOperationId,
        RuntimeToast,
    };
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (_shell, hosts, cx) = a_thread_of_a_beside_b(cx, 1200.);
    // The device took up the system's language.
    crate::set_locale(crate::LANGUAGE_ENGLISH);
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let toast = |host: &HostKey, toast: RuntimeToast, cx: &mut VisualTestContext| {
        send_events(
            &hosts[host].host,
            vec![(
                Topic::RuntimeEvents,
                ServerEvent::Runtime(RuntimeNotification::Toast(toast)),
            )],
        );
        draw(cx);
        cx.run_until_parked();
        draw(cx);
    };
    let says = |cx: &mut VisualTestContext, text: &str| {
        cx.debug_bounds(format!("toast-says-{text}").leak())
            .is_some()
    };
    toast(
        &key_b,
        RuntimeToast::ProviderUpdatesAvailable {
            updates: vec![ProviderUpdateAvailable {
                provider: agent::ProviderKind::Codex,
                version: "2.0.0".into(),
                automatic: true,
            }],
        },
        cx,
    );
    assert!(says(cx, "host-b: Update available: Codex v2.0.0"));
    let _ = sent(&hosts[&key_a].host);
    let _ = sent(&hosts[&key_b].host);
    click("toast-action", cx);
    let updates = |host: &HostKey| {
        sent(&hosts[host].host)
            .into_iter()
            .filter(|payload| {
                matches!(
                    payload,
                    ClientPayload::Command(Command::UpdateProviders { .. })
                )
            })
            .count()
    };
    assert_eq!((updates(&key_a), updates(&key_b)), (0, 1));

    let run = |completed| ProviderUpdateRun {
        total: 1,
        completed,
        current: Some(agent::ProviderKind::Codex),
        failed: Vec::new(),
    };
    for host in [&key_a, &key_b] {
        toast(
            host,
            RuntimeToast::ProviderUpdateStarted {
                operation: RuntimeOperationId(1),
                run: run(0),
            },
            cx,
        );
    }
    toast(
        &key_a,
        RuntimeToast::ProviderUpdateFinished {
            operation: RuntimeOperationId(1),
            run: run(1),
        },
        cx,
    );
    assert!(
        says(cx, "host-b: Updating Codex… 0/1") && !says(cx, "host-a: Updating Codex… 0/1"),
        "A's update finished and B's goes on"
    );

    // An update this window did not see start shows from its progress.
    toast(
        &key_a,
        RuntimeToast::ProviderUpdateProgress {
            operation: RuntimeOperationId(2),
            run: run(0),
        },
        cx,
    );
    assert!(
        says(cx, "host-a: Updating Codex… 0/1"),
        "A's second update shows from its progress"
    );
}

/// Stopping a plugin change from its toast ends the command the change
/// started, whatever that command was doing. The profile's harness is
/// set to a program that runs no harness: `/bin/sh`, which pi's install
/// arguments (`install <source>`) have run the script `install` in the
/// profile's home. The script opens the source, a FIFO this test reads,
/// and sleeps holding it, so the read ends only when that process ends.
#[cfg(unix)]
#[gpui::test]
fn stopping_a_plugin_change_from_its_toast_ends_the_command_it_started(cx: &mut TestAppContext) {
    use std::io::Read as _;
    use tcode_core::settings::ProfileSettingsPatch;
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale(crate::LANGUAGE_ENGLISH);
    // The change's toast spins while it runs, which rests only with
    // motion reduced.
    cx.update(|cx| cx.set_reduce_motion(true));
    let root = crate::store::tests::scratch_root("plugin-change-stop");
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("install"), "exec 3>\"$1\"\nexec sleep 86400\n").unwrap();
    let source = root.join("source");
    assert!(
        tcode_services::process::command("mkfifo")
            .arg(&source)
            .status()
            .unwrap()
            .success()
    );
    let disk = tcode_services::store::SessionStore::open_at(root.join("data")).unwrap();
    let (_shell, store, host, mut driver, cx) = mount_on_host(cx, disk, None);
    for patch in [
        ProfileSettingsPatch::SetBinaryPath(Some("/bin/sh".into())),
        ProfileSettingsPatch::SetHomePath(Some(home.clone())),
    ] {
        driver.command(
            cx,
            Command::UpdateProfileSettings {
                profile_id: "pi".into(),
                patch,
            },
        );
    }
    // A change is made to a listing; this one fails (`sh list`), as a
    // listing may.
    driver.command(
        cx,
        Command::RefreshProviderPlugins {
            profile_id: "pi".into(),
            cwd: None,
        },
    );
    driver.wait_until(cx, "pi's listing", |cx| {
        store.read_with(cx, |store, _| {
            store
                .provider_plugin_catalog("pi", None)
                .is_some_and(|catalog| !catalog.loading)
        })
    });
    driver.command(
        cx,
        Command::InstallProviderPlugin {
            profile_id: "pi".into(),
            entry_id: source.to_string_lossy().into_owned(),
            scope: agent::PluginScope::User,
            cwd: None,
        },
    );
    // Opening the FIFO returns once the command has opened it too.
    let mut held = std::fs::File::open(&source).unwrap();
    driver.settle_until(cx, "the change's toast", |cx| {
        cx.debug_bounds("toast-action").is_some()
    });
    click("toast-action", cx);
    let mut rest = Vec::new();
    held.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
}

/// A failed git action's toast carries git's own words, which Copy
/// error gives back as they came, and its Retry sends the host the
/// action that failed, as it was asked.
#[gpui::test]
fn a_failed_git_actions_toast_gives_its_detail_and_retries_the_same_action(
    cx: &mut TestAppContext,
) {
    use tcode_protocol::{GitActionRequest, RuntimeNotification, RuntimeOperationId, RuntimeToast};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (_shell, hosts, cx) = a_thread_of_a_beside_b(cx, 1200.);
    crate::set_locale(crate::LANGUAGE_ENGLISH);
    let host = &hosts[&HostKey::Remote("host-a".into())].host;
    let detail = "fatal: the remote end hung up unexpectedly\n  (see `git help push`)";
    let failed = GitActionRequest {
        session_id: "thread-1".into(),
        action: tcode_core::git::GitAction::CommitPush,
        message: Some("Ship the parser".into()),
        included: Some(vec!["src/parse.rs".into()]),
        feature_branch: Some("parser".into()),
    };
    send_events(
        host,
        vec![(
            Topic::RuntimeEvents,
            ServerEvent::Runtime(RuntimeNotification::Toast(RuntimeToast::GitFailed {
                operation: RuntimeOperationId(1),
                detail: detail.into(),
                retry: failed.clone(),
            })),
        )],
    );
    draw(cx);
    cx.run_until_parked();
    draw(cx);

    click("toast-copy", cx);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some(detail.to_owned())
    );
    let _ = sent(host);
    click("toast-action", cx);
    let retried: Vec<_> = sent(host)
        .into_iter()
        .filter_map(|payload| match payload {
            ClientPayload::Command(Command::RunGitAction {
                session_id,
                action,
                message,
                included,
                feature_branch,
            }) => Some(GitActionRequest {
                session_id,
                action,
                message,
                included,
                feature_branch,
            }),
            _ => None,
        })
        .collect();
    assert_eq!(retried, [failed]);
}

/// Over two hosts Settings opens on the current thread's host and
/// names a selector for the other: a host's setting changed there goes
/// to the host chosen alone, on the section the page was on, on a wide
/// window and on a phone's list. This device's rows are the same on
/// either host's page.
#[gpui::test]
fn settings_set_the_host_chosen_and_this_devices_rows_whichever_it_is(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, hosts, client, cx) = mount_workspace(cx, two_hosts(), Default::default(), 1200.);
    cx.update(|_, cx| crate::shortcut::init(cx));
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    for (key, store) in [(&key_a, &a), (&key_b, &b)] {
        hosts[key].come_up(store, Vec::new(), None, cx);
        send_events(&hosts[key].host, vec![panes_index(&[])]);
    }
    serve_thread(&hosts[&key_b].host, "thread-1");
    b.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_hosts(&shell, cx), std::slice::from_ref(&key_b));
    heard(&hosts[&key_a], cx);
    heard(&hosts[&key_b], cx);
    let patches = |host: &WorkspaceHost| {
        sent(&host.host)
            .into_iter()
            .filter(|payload| {
                matches!(
                    payload,
                    ClientPayload::Command(Command::PatchSettings { .. })
                )
            })
            .count()
    };

    shell
        .read_with(cx, |shell, _| shell.window_state())
        .update(cx, |state, cx| state.open_settings(cx));
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("settings-machine-caption").is_none());
    click("settings-nav-threads", cx);
    click("delete-confirm", cx);
    assert_eq!((patches(&hosts[&key_a]), patches(&hosts[&key_b])), (0, 1));

    click("settings-host-selector", cx);
    for host in ["settings-host-host-a", "settings-host-host-b"] {
        assert!(cx.debug_bounds(host).is_some(), "{host}");
    }
    click("settings-host-host-a", cx);
    crate::test_frames::draw_settled(cx);
    click("delete-confirm", cx);
    assert_eq!((patches(&hosts[&key_a]), patches(&hosts[&key_b])), (1, 0));

    // The device's name, typed on A's page and then on B's, is one name.
    let device_name = || client.preferences.borrow().device_name.clone();
    click("settings-nav-general", cx);
    click("device-name-field", cx);
    cx.simulate_input(" desk");
    assert_eq!(device_name().as_deref(), Some("tablet desk"));
    click("settings-host-selector", cx);
    click("settings-host-host-b", cx);
    crate::test_frames::draw_settled(cx);
    click("device-name-field", cx);
    cx.simulate_input("!");
    assert_eq!(device_name().as_deref(), Some("tablet desk!"));
    assert_eq!((patches(&hosts[&key_a]), patches(&hosts[&key_b])), (0, 0));

    // A phone's list of sections chooses the same way.
    resize(cx, 393.);
    crate::test_frames::draw_settled(cx);
    click("settings-host-selector", cx);
    click("settings-host-host-a", cx);
    click("settings-nav-threads", cx);
    crate::test_frames::draw_settled(cx);
    click("delete-confirm", cx);
    assert_eq!((patches(&hosts[&key_a]), patches(&hosts[&key_b])), (1, 0));

    // A host that refuses the device is not one to set: A's rail names
    // A alone.
    hosts[&key_b]
        .host
        .states
        .try_send(tcode_client::ConnectionState::Offline {
            reason: tcode_client::ConnectionFailure::AuthenticationRejected,
        })
        .unwrap();
    resize(cx, 1200.);
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("settings-host-selector").is_none());
    assert!(cx.debug_bounds("settings-machine-caption").is_some());
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

/// Over several hosts, with no thread current, "+" asks neither host
/// anything: it offers each host's projects under the host's name, and
/// the project picked is drafted on its own host, where the column goes.
#[gpui::test]
fn a_new_thread_with_none_current_starts_on_the_host_picked(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, _client, cx) = mount_workspace(cx, two_hosts(), Default::default(), 393.);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    let project = |id: &str| crate::store::tests::project_at(id, std::path::Path::new(PANES_CWD));
    hosts[&key_a].come_up(&a, Vec::new(), None, cx);
    hosts[&key_b].come_up(&b, vec![project("on-b")], None, cx);
    crate::test_frames::draw_settled(cx);
    assert_eq!(store_of(&shell, cx), a);
    assert_eq!(shell.read_with(cx, |shell, cx| shell.page_thread(cx)), None);
    heard(&hosts[&key_a], cx);
    heard(&hosts[&key_b], cx);

    let plus = cx.debug_bounds("compact-new-thread").expect("Threads' +");
    cx.simulate_click(plus.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("tcode-popup-menu").is_some(), "a chooser");
    assert!(!shell.read_with(cx, |shell, cx| shell.window_state.read(cx).palette_open));
    assert_eq!(sent(&hosts[&key_a].host), Vec::new(), "A is asked nothing");
    assert_eq!(sent(&hosts[&key_b].host), Vec::new(), "B is asked nothing");

    // A's name and quick thread, then B's name, quick thread and project.
    let on_b = cx.debug_bounds("menu-item-5").expect("B's project");
    cx.simulate_click(on_b.center(), gpui::Modifiers::default());
    draw(cx);
    assert_eq!(drafts_asked(&hosts[&key_a]), Vec::<Option<String>>::new());
    ack_project_draft(&hosts[&key_b].host, "on-b", "draft");
    cx.run_until_parked();
    crate::test_frames::draw_settled(cx);
    assert_eq!(store_of(&shell, cx), b, "the column goes to B");
    assert_eq!(pane_threads(&shell, cx), [Some("draft".into())]);
}

/// The one key that starts a thread, a split, drafts beside the current
/// thread on that thread's host and in its project; the other host
/// hears nothing.
#[gpui::test]
fn a_split_drafts_on_the_current_threads_host(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, _client, cx) = mount_workspace(cx, two_hosts(), Default::default(), 1200.);
    cx.update(|_, cx| crate::shortcut::init(cx));
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    let project = |id: &str| crate::store::tests::project_at(id, std::path::Path::new(PANES_CWD));
    hosts[&key_a].come_up(&a, Vec::new(), None, cx);
    hosts[&key_b].come_up(&b, Vec::new(), None, cx);
    let mut meta =
        tcode_core::project::SessionMeta::new(agent::ProviderKind::Codex, PANES_CWD.into(), None);
    meta.id = "thread-1".into();
    meta.title = "thread-1".into();
    meta.project_id = Some("on-b".into());
    send_events(
        &hosts[&key_b].host,
        vec![(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions: vec![meta],
                projects: vec![project("on-b")],
            }),
        )],
    );
    serve_thread(&hosts[&key_b].host, "thread-1");
    b.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    heard(&hosts[&key_b], cx);
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell.page_thread(cx)),
        Some(ThreadKey {
            host: key_b.clone(),
            session_id: "thread-1".into(),
        })
    );
    heard(&hosts[&key_a], cx);
    heard(&hosts[&key_b], cx);

    cx.simulate_keystrokes("secondary-d");
    draw(cx);
    assert_eq!(drafts_asked(&hosts[&key_b]), [Some("on-b".to_owned())]);
    assert_eq!(drafts_asked(&hosts[&key_a]), Vec::<Option<String>>::new());
}

/// A draft asked for on a host the column has since left, for a thread
/// asked for on another, lands nowhere when it is answered: the user
/// moved on, and the column stays where they went.
#[gpui::test]
fn a_draft_answered_after_the_column_left_its_host_stays_off(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, _client, cx) = mount_workspace(cx, two_hosts(), Default::default(), 1200.);
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    hosts[&key_a].come_up(&a, Vec::new(), None, cx);
    hosts[&key_b].come_up(&b, Vec::new(), None, cx);
    send_events(&hosts[&key_b].host, vec![panes_index(&[])]);
    serve_thread(&hosts[&key_b].host, "thread-1");
    assert_eq!(store_of(&shell, cx), a);

    a.update(cx, |store, cx| store.start_scratch_draft(cx));
    b.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    draw(cx);
    assert_eq!(store_of(&shell, cx), b);

    ack_draft(&hosts[&key_a].host, "late");
    cx.run_until_parked();
    crate::test_frames::draw_settled(cx);
    assert_eq!(store_of(&shell, cx), b, "the column stays on B");
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    assert!(!a.read_with(cx, |store, _| store.is_thread_open("late")));
}

/// Re-pairing with a machine ends on `open_host` with that machine's id.
/// The link it refused has stopped reconnecting, so it is rebuilt rather
/// than kept saying the pairing was rejected; a pairing into one of its
/// spaces is likewise a new link, not the full one it replaces.
#[gpui::test]
fn repairing_rebuilds_a_link_the_machine_rejected(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, client, cx) = mount_restored(cx, &["threads"], true);
    let fresh_transport = || {
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let (_states, state) = async_channel::unbounded();
        Transport {
            originals: None,
            to_host: to_host.into(),
            from_host,
            state,
            current_host: None,
        }
    };
    let rejected = tcode_client::ConnectionState::Offline {
        reason: tcode_client::ConnectionFailure::AuthenticationRejected,
    };
    host.states.send_blocking(rejected.clone()).unwrap();
    cx.run_until_parked();
    assert_eq!(
        store_of(&shell, cx).read_with(cx, |store, _| store.connection_state()),
        rejected
    );
    let saved = client.saved.clone();
    client.transports.borrow_mut().push(fresh_transport());
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_host(
                AttachmentTarget::Remote(Box::new(saved.clone())),
                window,
                cx,
            )
        });
    });
    cx.run_until_parked();
    assert!(
        client.transports.borrow().is_empty(),
        "the refused link is replaced"
    );
    assert_ne!(
        store_of(&shell, cx).read_with(cx, |store, _| store.connection_state()),
        rejected
    );

    // The working link to the same machine is kept...
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_host(
                AttachmentTarget::Remote(Box::new(saved.clone())),
                window,
                cx,
            )
        });
    });
    cx.run_until_parked();

    // ...and joining one of its spaces is a different link.
    client.transports.borrow_mut().push(fresh_transport());
    let member = tcode_client::pairing::PairedHost {
        space_id: Some("space-1".into()),
        space_name: Some("Shared".into()),
        ..saved
    };
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_host(AttachmentTarget::Remote(Box::new(member)), window, cx)
        });
    });
    cx.run_until_parked();
    assert!(
        client.transports.borrow().is_empty(),
        "a space membership replaces the full link"
    );
}

#[gpui::test]
fn pending_outbox_overrides_desktop_launch_without_a_snapshot(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    struct SavedOutbox;
    impl tcode_client::outbox::Storage for SavedOutbox {
        fn load(&self) -> Result<Vec<tcode_client::outbox::Entry>, tcode_protocol::ProtocolError> {
            Ok(vec![tcode_client::outbox::Entry {
                key: "saved-send".into(),
                command: Command::SendTurn {
                    session_id: "pending-thread".into(),
                    text: "survives relaunch".into(),
                    attachment_paths: Vec::new(),
                },
            }])
        }
        fn save(
            &self,
            _: &[tcode_client::outbox::Entry],
        ) -> Result<(), tcode_protocol::ProtocolError> {
            Ok(())
        }
    }
    cx.update(crate::test_client::init);
    cx.update(crate::markdown::init);
    let (to_host, _outgoing) = async_channel::unbounded();
    let (_incoming, from_host) = async_channel::unbounded();
    let (_state_tx, state) = async_channel::unbounded();
    let client = Rc::new(ReturningClient {
        saved: tcode_client::pairing::PairedHost {
            host_id: "pending-host".into(),
            name: "Pending host".into(),
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["127.0.0.1:48442".into()],
            last_connected_unix: None,
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
        preferences: RefCell::new(Default::default()),
        machine_exists: true,
        outbox: Some(std::sync::Arc::new(SavedOutbox)),
    });
    cx.update(|cx| crate::test_client::attach(client, true, cx));
    let (shell, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|_| WindowState::default());
        AppShell::new(
            state,
            ShellSetup {
                initial: Some(AttachmentTarget::Local),
                restore_navigation: false,
                ..Default::default()
            },
            window,
            cx,
        )
    });
    cx.simulate_resize(size(px(1024.), px(700.)));
    draw(cx);
    let store = store_of(&shell, cx);
    store.read_with(cx, |store, cx| {
        assert_eq!(store.remote_host_name(), Some("Pending host"));
        assert!(store.is_thread_open("pending-thread"));
        assert!(!store.chat_loading_for(Some("pending-thread"), cx));
        assert_eq!(
            store.delivery_messages_for(Some("pending-thread"), cx)[0].1,
            "survives relaunch"
        );
        assert!(store.sidebar_sessions().is_empty());
    });
    assert!(cx.debug_bounds("pending-delivery-bubble").is_some());
    assert!(cx.debug_bounds("pending-thread-row").is_some());
    as_mobile(cx);
    cx.simulate_resize(size(px(393.), px(852.)));
    let navigation = shell.read_with(cx, |shell, _| shell.window_state());
    navigation.update(cx, |state, cx| state.enter_workspace(cx));
    draw(cx);
    shell.read_with(cx, |shell, cx| {
        assert!(shell.compact(cx));
        assert_eq!(shell.destination(cx), Destination::Threads);
    });
    assert!(
        cx.debug_bounds("pending-thread-row").is_some(),
        "compact Threads exposes the outbox without a cached index"
    );
    navigation.update(cx, |state, cx| state.go(Destination::Thread, cx));
    draw(cx);
    assert!(cx.debug_bounds("pending-delivery-bubble").is_some());
}

/// A message on its way to one thread shows in the pane showing that
/// thread, whichever pane has the keyboard, and in no other.
#[gpui::test]
fn a_pending_delivery_shows_only_in_its_threads_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.simulate_keystrokes("secondary-d");
    serve_thread(&host, "thread-2");
    cx.simulate_keystrokes("secondary-2");
    await_restore_update(&shell, cx, |store, cx| {
        store.is_thread_open("thread-2") && !store.chat_loading_for(Some("thread-2"), cx)
    });
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    store_of(&shell, cx).update(cx, |store, _| {
        store.send_turn_for("thread-2".into(), "On its way".into(), Vec::new())
    });
    let pane_of = |cx: &mut VisualTestContext| {
        let bubble = cx
            .debug_bounds("pending-delivery-bubble")
            .expect("the message on its way");
        shell.read_with(cx, |shell, cx| {
            shell
                .column
                .as_ref()
                .expect("attached")
                .panes(cx)
                .iter()
                .find(|pane| pane.read(cx).bounds().contains(&bubble.center()))
                .and_then(|pane| pane.read(cx).session_id().map(str::to_owned))
        })
    };
    for (key, focused) in [("secondary-2", "thread-2"), ("secondary-1", "thread-1")] {
        cx.simulate_keystrokes(key);
        await_restore_update(&shell, cx, |store, _| store.is_thread_open(focused));
        crate::test_frames::draw_settled(cx);
        assert_eq!(
            pane_of(cx).as_deref(),
            Some("thread-2"),
            "with {focused} focused"
        );
    }
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

/// A phone that goes back to the thread list before the thread's
/// conversation arrives keeps the thread selected, so the conversation
/// still loads behind the list. That is not a read; showing the thread
/// page again is.
#[gpui::test]
fn a_conversation_that_loads_behind_the_thread_list_is_not_read(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads", "thread"], true);
    let reads = |host: &MountedShell| {
        sent(host)
            .into_iter()
            .filter_map(|payload| match payload {
                ClientPayload::Command(Command::MarkSessionRead { session_id, .. }) => {
                    Some(session_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    host.states
        .try_send(tcode_client::ConnectionState::Syncing { path: None })
        .unwrap();
    draw(cx);
    assert!(cx.debug_bounds("compact-thread-page").is_some());
    assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
    draw(cx);

    restore_index(&shell, &host, true, cx);
    restore_status(&shell, &host, cx);
    restore_settings_and_events(&shell, &host, cx);
    assert_eq!(reads(&host), Vec::<String>::new());

    shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
    draw(cx);
    assert_eq!(reads(&host), ["thread-a"]);
}

/// The phone's direct path dying is reported by the transport as a new
/// value of the state it is in naming the relay; that alone, with no
/// host line and no touch, changes what the nav bar says about the
/// link. A path known while the baseline still loads is already shown
/// as the connected path: syncing is what the skeletons say, not the
/// subtitle.
#[gpui::test]
fn a_path_change_alone_updates_the_connection_subtitle(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads"], true);
    let store = store_of(&shell, cx);
    let subtitle = |cx: &mut VisualTestContext| {
        draw(cx);
        store.read_with(cx, |store, _| {
            crate::remote::connection_label(&store.connection_state())
        })
    };
    let lan = tcode_protocol::PathInfo {
        direct: true,
        relay: None,
        lan: true,
        probing_direct: false,
    };
    assert_eq!(subtitle(cx), "Connected", "the baseline is not in yet");
    host.states
        .try_send(tcode_client::ConnectionState::Syncing {
            path: Some(lan.clone()),
        })
        .unwrap();
    assert_eq!(subtitle(cx), "LAN · Connected");
    restore_index(&shell, &host, true, cx);
    restore_status(&shell, &host, cx);
    restore_settings_and_events(&shell, &host, cx);
    assert_eq!(subtitle(cx), "LAN · Connected", "the baseline is in");
    host.states
        .try_send(tcode_client::ConnectionState::Connected { path: Some(lan) })
        .unwrap();
    assert_eq!(subtitle(cx), "LAN · Connected");
    host.states
        .try_send(tcode_client::ConnectionState::Connected {
            path: Some(tcode_protocol::PathInfo {
                direct: false,
                relay: Some("https://aps1-1.relay.n0.iroh.link/".into()),
                lan: false,
                probing_direct: false,
            }),
        })
        .unwrap();
    assert_eq!(subtitle(cx), "Relay (aps1-1) · Connected");
    assert!(
        cx.debug_bounds("compact-threads-page").is_some(),
        "the Threads page is what shows it"
    );
}

/// A desktop window opened in place of one that closed — macOS keeps the
/// app and its local host running when the window closes, and the Dock
/// opens it again — is where the closed one was: on the same host and on
/// the same thread.
#[gpui::test]
fn a_reopened_window_is_where_the_closed_one_was(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::test_client::init);
    let scripted = || {
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let (states, state) = async_channel::unbounded();
        (
            Transport {
                originals: None,
                to_host: to_host.into(),
                from_host,
                state,
                current_host: None,
            },
            MountedShell {
                outgoing,
                observed: RefCell::new(Vec::new()),
                incoming,
                states,
            },
        )
    };
    let (first, closed_host) = scripted();
    let (second, reopened_host) = scripted();
    let transports = Rc::new(RefCell::new(vec![first, second]));
    let local: crate::attachment::LocalTransport = {
        let transports = transports.clone();
        Rc::new(move |_| Ok(transports.borrow_mut().remove(0)))
    };
    let open = |setup: ShellSetup, cx: &mut TestAppContext| {
        let window = cx.open_window(size(px(1200.), px(800.)), move |window, cx| {
            let state = cx.new(|_| WindowState::default());
            AppShell::new(state, setup, window, cx)
        });
        let shell = window.root(cx).unwrap();
        let mut window_cx = VisualTestContext::from_window(window.into(), cx);
        draw(&mut window_cx);
        (shell, window_cx)
    };

    let (closed, mut closed_cx) = open(
        ShellSetup {
            local: Some(local.clone()),
            initial: Some(AttachmentTarget::Local),
            ..Default::default()
        },
        cx,
    );
    restore_index(&closed, &closed_host, true, &mut closed_cx);
    store_of(&closed, &closed_cx).update(&mut closed_cx, |store, cx| {
        store.show_thread("thread-a".into(), cx)
    });
    draw(&mut closed_cx);
    assert_eq!(
        closed.read_with(&closed_cx, |shell, cx| shell
            .page_thread(cx)
            .map(|thread| thread.session_id)),
        Some("thread-a".into())
    );

    // As desktop bootstrap does once the window has gone.
    closed_cx.update(|window, _| window.remove_window());
    closed_cx.run_until_parked();
    let place = closed.read_with(cx, |shell, cx| shell.current_place(cx));
    closed.update(cx, |shell, cx| shell.close_links(cx));

    let (reopened, mut reopened_cx) = open(
        ShellSetup {
            local: Some(local),
            reopen: Some(place),
            ..Default::default()
        },
        cx,
    );
    assert!(
        transports.borrow().is_empty(),
        "the reopened window attaches to the local host again"
    );
    restore_index(&reopened, &reopened_host, true, &mut reopened_cx);
    reopened.read_with(&reopened_cx, |shell, cx| {
        assert!(!shell.store(cx).unwrap().read(cx).is_remote());
        assert_eq!(
            shell
                .page_thread(cx)
                .map(|thread| thread.session_id)
                .as_deref(),
            Some("thread-a")
        );
    });
}

/// A phone suspended before the host acknowledged a read receipt keeps
/// it in its outbox. The next launch returns to the threads the phone
/// was left on: only a message on its way opens its thread.
#[gpui::test]
fn a_saved_read_receipt_does_not_open_its_thread_at_launch(cx: &mut TestAppContext) {
    struct SavedReadReceipt;
    impl tcode_client::outbox::Storage for SavedReadReceipt {
        fn load(&self) -> Result<Vec<tcode_client::outbox::Entry>, tcode_protocol::ProtocolError> {
            Ok(vec![tcode_client::outbox::Entry {
                key: "read-receipt".into(),
                command: Command::MarkSessionRead {
                    session_id: "thread-b".into(),
                    through: 1,
                },
            }])
        }
        fn save(
            &self,
            _: &[tcode_client::outbox::Entry],
        ) -> Result<(), tcode_protocol::ProtocolError> {
            Ok(())
        }
    }
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::window_seam::force_mobile_layout);
    let (shell, _, _, cx) = mount_restored_with_outbox(
        cx,
        &["threads"],
        true,
        393.,
        "plan",
        Some(std::sync::Arc::new(SavedReadReceipt)),
    );
    shell.read_with(cx, |shell, cx| {
        assert_eq!(
            shell.window_state.read(cx).history(),
            [Destination::Threads]
        );
        assert_ne!(
            shell
                .page_thread(cx)
                .map(|thread| thread.session_id)
                .as_deref(),
            Some("thread-b")
        );
    });
    assert!(cx.debug_bounds("pending-thread-row").is_none());
}

#[gpui::test]
fn a_cold_start_restores_the_thread_once_without_raising_the_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::window_seam::force_mobile_layout);
    let (shell, host, _, cx) =
        mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 393., "plan");
    assert!(
        cx.debug_bounds("baseline-loading").is_some(),
        "the restored destination shows a skeleton before its Index baseline"
    );
    shell.read_with(cx, |shell, cx| {
        assert_eq!(
            shell.window_state.read(cx).history(),
            [Destination::Threads, Destination::Thread]
        );
        assert_eq!(
            shell
                .page_thread(cx)
                .map(|thread| thread.session_id)
                .as_deref(),
            Some("thread-a")
        );
    });
    restore_index(&shell, &host, true, cx);
    restore_index(&shell, &host, true, cx);
    restore_status(&shell, &host, cx);
    draw(cx);
    let subscriptions = sent(&host).into_iter().filter(|payload| matches!(payload,
            ClientPayload::Subscribe(subscription) if subscription.topic == Topic::SessionEvents { session_id: "thread-a".into() }
        )).count();
    assert_eq!(
        subscriptions, 1,
        "baseline reconciliation must not select again"
    );
    for (topic, event) in [
        (
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
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
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-a"), cx)
    });
    draw(cx);
    shell.read_with(cx, |shell, cx| {
        assert_eq!(
            shell.window_state.read(cx).history().last(),
            Some(&Destination::Thread)
        )
    });
    assert_eq!(
        typed_reaches(&host, cx),
        Vec::<String>::new(),
        "a cold restore must not open the keyboard after hydration"
    );
    let card = cx
        .debug_bounds("composer-card")
        .expect("the thread's composer");
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    assert_eq!(typed_reaches(&host, cx), ["thread-a"], "a tap opens it");
}

#[gpui::test]
fn cold_start_discards_transient_settings_and_pair_pages(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    for (saved, expected) in [
        (
            vec!["hosts", "threads", "thread", "settings", "settings_section"],
            vec![Destination::Threads, Destination::Thread],
        ),
        (
            vec!["hosts", "threads", "hosts", "pair"],
            vec![Destination::Threads, Destination::Hosts],
        ),
    ] {
        let (shell, _, _, cx) = mount_restored(cx, &saved, true);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.window_state.read(cx).history(), expected);
        });
    }
}

#[gpui::test]
fn cold_start_discards_a_missing_session_after_index_in_either_delivery_order(
    cx: &mut TestAppContext,
) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    for status_first in [false, true] {
        let (shell, host, client, cx) =
            mount_restored(cx, &["hosts", "threads", "thread", "panel"], true);
        assert!(
            cx.debug_bounds("baseline-loading").is_some(),
            "the restored Panel also waits behind a skeleton"
        );
        shell.read_with(cx, |shell, cx| {
            assert_eq!(
                shell.window_state.read(cx).history().last(),
                Some(&Destination::Panel)
            )
        });
        if status_first {
            restore_status(&shell, &host, cx);
        }
        restore_index(&shell, &host, false, cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(
                shell.window_state.read(cx).history(),
                [Destination::Threads]
            );
            assert_eq!(shell.page_thread(cx).map(|thread| thread.session_id), None);
        });
        cx.executor().advance_clock(Duration::from_millis(151));
        draw(cx);
        let preferences = client.load_preferences().unwrap();
        assert_eq!(
            preferences.navigation.unwrap()["history"],
            serde_json::json!(["threads"])
        );
    }
}

#[gpui::test]
fn a_cold_start_restores_the_saved_panel_tab(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::window_seam::force_mobile_layout);
    let (saved_panel, expected_tab) = ("plan", RightTab::Plan);
    let (shell, host, client, cx) = mount_restored_at_width(
        cx,
        &["hosts", "threads", "thread", "panel"],
        true,
        393.,
        saved_panel,
    );
    restore_index(&shell, &host, true, cx);
    restore_status(&shell, &host, cx);
    assert!(
        cx.debug_bounds("baseline-loading").is_some(),
        "status alone is not the conversation baseline"
    );
    let settings = tcode_core::settings::Settings {
        language: Some(crate::LANGUAGE_ENGLISH.into()),
        ..Default::default()
    };
    for (topic, event) in [
        (Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
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
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-a"), cx)
    });
    shell.read_with(cx, |shell, cx| {
        assert_eq!(
            shell.window_state.read(cx).history().last(),
            Some(&Destination::Panel)
        );
        assert_eq!(
            shell
                .store(cx)
                .unwrap()
                .read(cx)
                .panel_state_for(Some("thread-a"))
                .right_tab,
            expected_tab
        );
    });
    cx.executor().advance_clock(Duration::from_millis(151));
    draw(cx);
    assert_eq!(
        client.load_preferences().unwrap().navigation.unwrap()["panel"],
        saved_panel
    );
}

#[gpui::test]
fn navigation_is_saved_with_the_thread_and_history_and_survives_a_theme_edit(
    cx: &mut TestAppContext,
) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _, client, cx) = mount_restored(cx, &["hosts", "threads", "thread"], true);
    let store = store_of(&shell, cx);
    // Showing a thread on the existing page must checkpoint without
    // waiting for a host response or a changed history.
    store.update(cx, |store, cx| store.show_thread("thread-b".into(), cx));
    cx.executor().advance_clock(Duration::from_millis(151));
    draw(cx);
    assert_eq!(
        client.load_preferences().unwrap().navigation.unwrap(),
        serde_json::json!({
            "history": ["threads", "thread"],
            "thread": { "host": "last-host", "session_id": "thread-b" },
            "panel": "diff"
        })
    );

    store.update(cx, |store, cx| {
        store.show_thread("thread-c".into(), cx);
    });
    shell
        .read_with(cx, |shell, _| shell.window_state())
        .update(cx, |state, cx| state.go(Destination::Settings, cx));
    cx.executor().advance_clock(Duration::from_millis(151));
    draw(cx);
    let navigation = client.load_preferences().unwrap().navigation.unwrap();
    assert_eq!(
        navigation["thread"],
        serde_json::json!({ "host": "last-host", "session_id": "thread-c" })
    );
    assert_eq!(
        navigation["history"],
        serde_json::json!(["threads", "thread", "settings"])
    );
    // A theme edit keeps the history and selection the checkpoint saved.
    cx.update(|window, cx| {
        DevicePreferences::set_theme(tcode_core::settings::ThemeMode::Light, window, cx)
    });
    assert_eq!(
        client.load_preferences().unwrap().navigation,
        Some(navigation)
    );
    assert_eq!(
        client.load_preferences().unwrap().appearance,
        Some(tcode_core::settings::ThemeMode::Light)
    );
}

#[gpui::test]
fn cold_start_with_a_missing_machine_restores_the_empty_threads(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads", "thread"], false);
    shell.read_with(cx, |shell, cx| {
        assert_eq!(
            shell.window_state.read(cx).history(),
            [Destination::Threads]
        );
        assert!(shell.store(cx).is_none());
    });
    assert!(cx.debug_bounds("no-hosts").is_some());
    assert!(sent(&host).is_empty());
}

/// A tablet in landscape with no host is the threads' empty root and the
/// pages it leads to, nothing else: the wide layout has no local host and
/// no sidebar to switch routes with, so each page is the whole window
/// until the window has a host, Back returns to the root, and leaving
/// the last host comes back to it.
#[gpui::test]
fn a_wide_mobile_window_without_a_host_shows_the_empty_threads_until_one_opens(
    cx: &mut TestAppContext,
) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::window_seam::force_mobile_layout);
    let (shell, _host, client, cx) = mount_restored_at_width(cx, &["hosts"], false, 1024., "plan");
    let assert_on_empty_root = |cx: &mut VisualTestContext| {
        crate::test_frames::draw_settled(cx);
        assert!(
            cx.debug_bounds("no-hosts").is_some(),
            "the empty threads are the window"
        );
        assert!(cx.debug_bounds("chat-route").is_none(), "no workspace");
        assert!(
            cx.debug_bounds("sidebar-feature-hosts").is_none(),
            "no sidebar"
        );
        assert!(
            cx.debug_bounds("compact-back").is_none(),
            "nothing to go back to"
        );
    };
    shell.read_with(cx, |shell, cx| {
        assert!(!shell.compact(cx), "1024px on a mobile build is wide");
        assert!(shell.store(cx).is_none());
        assert_eq!(
            shell.window_state.read(cx).history(),
            [Destination::Threads]
        );
    });
    assert_on_empty_root(cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| assert!(!shell.back(window, cx)));
    });
    assert_on_empty_root(cx);
    // Machines and Pair are pages over the root, and Back returns to it.
    let machines = cx
        .debug_bounds("no-hosts-machines")
        .expect("the root leads to Machines");
    cx.simulate_click(machines.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("hosts-page").is_some());
    let back = cx
        .debug_bounds("compact-back")
        .expect("Machines returns to the threads");
    cx.simulate_click(back.center(), gpui::Modifiers::default());
    assert_on_empty_root(cx);
    let pair = cx
        .debug_bounds("no-hosts-pair")
        .expect("the root leads to pairing");
    cx.simulate_click(pair.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    let back = cx
        .debug_bounds("compact-back")
        .expect("Pair returns to the threads");
    cx.simulate_click(back.center(), gpui::Modifiers::default());
    assert_on_empty_root(cx);

    // Opening a host leaves the root for its workspace...
    let saved = client.saved.clone();
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_host(AttachmentTarget::Remote(Box::new(saved)), window, cx)
        });
    });
    crate::test_frames::draw_settled(cx);
    shell.read_with(cx, |shell, cx| {
        assert!(shell.store(cx).is_some());
        assert_eq!(shell.window_state.read(cx).route(), Route::Chat);
    });
    assert!(cx.debug_bounds("no-hosts").is_none());
    assert!(
        cx.debug_bounds("sidebar-feature-hosts").is_some(),
        "the wide workspace"
    );

    // ...and leaving it returns to the root.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.leave_host(&HostKey::Remote("last-host".into()), window, cx)
        });
    });
    shell.read_with(cx, |shell, cx| {
        assert!(shell.store(cx).is_none());
        assert_eq!(
            shell.window_state.read(cx).history(),
            [Destination::Threads]
        );
    });
    assert_on_empty_root(cx);
}

/// Navigation never raises a software keyboard: on a phone every route
/// leaves the composer unfocused.
#[gpui::test]
fn navigation_dismisses_mobile_keyboards_but_preserves_explicit_editing_focus(
    cx: &mut TestAppContext,
) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    as_mobile(cx);
    resize(cx, 393.);
    let store = store_of(&shell, cx);
    let tap_composer = |cx: &mut VisualTestContext| {
        crate::test_frames::draw_settled(cx);
        let card = cx
            .debug_bounds("composer-card")
            .expect("the thread's composer");
        cx.simulate_click(card.center(), gpui::Modifiers::default());
        draw(cx);
    };
    // The host answers the thread shown, so its composer is drawn.
    let show = |id: &str, cx: &mut VisualTestContext| {
        store.update(cx, |store, cx| store.show_thread(id.into(), cx));
        serve_thread(&host, id);
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some(id), cx)
        });
        draw(cx);
    };
    let unraised = Vec::<String>::new();
    for width in [1024., 393.] {
        resize(cx, width);
        for id in ["thread-1", "thread-2"] {
            if id == "thread-2" {
                tap_composer(cx);
            }
            show(id, cx);
            assert_eq!(typed_reaches(&host, cx), unraised, "showing {id}");
        }
        if shell.read_with(cx, |shell, cx| shell.compact(cx)) {
            tap_composer(cx);
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
            });
        }
        draw(cx);
        show("thread-2", cx);
        assert_eq!(typed_reaches(&host, cx), unraised, "back and in again");
        for destination in [
            Destination::Panel,
            Destination::Hosts,
            Destination::Settings,
        ] {
            tap_composer(cx);
            shell.update(cx, |shell, cx| shell.go(destination, cx));
            draw(cx);
            show("thread-2", cx);
            assert_eq!(
                typed_reaches(&host, cx),
                unraised,
                "{destination:?} and back"
            );
        }
    }
    tap_composer(cx);
    for width in [1024., 393.] {
        resize(cx, width);
        draw(cx);
        assert_eq!(typed_reaches(&host, cx), ["thread-2"], "at {width}");
    }
}

#[gpui::test]
fn keyboard_cover_changes_keep_editor_focus_until_explicit_dismissal(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::test_client::init);
    cx.update(crate::window_seam::force_mobile_layout);
    let (shell, host, _, cx) =
        mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 393., "plan");
    restore_index(&shell, &host, true, cx);
    restore_status(&shell, &host, cx);
    for (topic, event) in [
        (
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        ),
        (
            Topic::SessionEvents {
                session_id: "thread-a".into(),
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
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-a"), cx)
    });
    crate::test_frames::draw_settled(cx);
    let card = cx
        .debug_bounds("composer-card")
        .expect("composer on the thread page");
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    draw(cx);
    crate::test_frames::occlude(
        cx,
        gpui::Edges {
            bottom: px(400.),
            ..Default::default()
        },
    );
    draw(cx);
    crate::test_frames::occlude(cx, gpui::Edges::default());
    draw(cx);
    // Hardware input hides the software keyboard without ending editing.
    cx.simulate_input("keyboard draft");
    let card = cx
        .debug_bounds("composer-card")
        .expect("composer on the thread page");
    cx.simulate_click(
        gpui::point(card.center().x, card.top() - px(40.)),
        gpui::Modifiers::default(),
    );
    draw(cx);
    // A tap on the blank timeline dismisses editing.
    cx.simulate_input(" and more");
    let _ = sent(&host);
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    draw(cx);
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(
        turns_sent(&host),
        [("thread-a".to_owned(), "keyboard draft".to_owned())],
        "a tap on the composer resumes editing"
    );
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

/// A tablet that rotates to a phone's width and back: a subagent's
/// thread draws its model, effort, context and permission in one row,
/// with no card, field, send or settings drawer; nothing pressed or
/// typed there sends, and the
/// lead's draft is intact when its thread is reopened.
#[gpui::test]
fn a_subagent_thread_composer_is_its_settings_read_only(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::test_client::init);
    cx.update(crate::window_seam::force_mobile_layout);
    let (shell, host, _, cx) =
        mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 1200., "plan");
    restore_index(&shell, &host, true, cx);
    restore_status(&shell, &host, cx);
    let show = |cx: &mut VisualTestContext| {
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        crate::test_frames::draw_settled(cx);
    };
    // A software keyboard waits for an explicit tap on the composer.
    let tap_field = |cx: &mut VisualTestContext| {
        let card = cx.debug_bounds("composer-card").expect("parent composer");
        cx.simulate_click(
            gpui::point(card.center().x, card.top() + px(24.)),
            gpui::Modifiers::default(),
        );
        draw(cx);
    };
    deliver_thread(&shell, &host, "thread-a", false, cx);
    crate::test_frames::draw_settled(cx);
    tap_field(cx);
    cx.simulate_input("parent draft");
    draw(cx);
    for width in [1200., 393.] {
        deliver_thread(&shell, &host, "thread-a", false, cx);
        resize(cx, width);
        show(cx);
        assert!(cx.debug_bounds("composer-card").is_some());

        deliver_thread(&shell, &host, "child", true, cx);
        show(cx);
        sent(&host);
        for absent in [
            "composer-card",
            "composer-settings-drawer",
            "send-message",
            "stop-turn",
            "steer-turn",
            "attach-menu",
            "overflow-controls",
        ] {
            assert!(
                cx.debug_bounds(absent).is_none(),
                "{absent} is drawn at {width}"
            );
        }
        let row = cx
            .debug_bounds("composer-subagent")
            .expect("the subagent's settings");
        for selector in [
            "model-picker",
            "traits-chip",
            "context-meter",
            "permission-chip",
        ] {
            let bounds = cx.debug_bounds(selector).expect(selector);
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            draw(cx);
            for pane in ["permission-pane", "context-pane"] {
                assert!(
                    cx.debug_bounds(pane).is_none(),
                    "{selector} opened {pane} at {width}"
                );
            }
        }
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        cx.simulate_keystrokes("x shift-tab enter");
        draw(cx);
        assert!(
            sent(&host)
                .iter()
                .all(|payload| !matches!(payload, ClientPayload::Command(_))),
            "a subagent's composer must not send commands"
        );
    }
    deliver_thread(&shell, &host, "thread-a", false, cx);
    resize(cx, 1200.);
    tap_field(cx);
    cx.simulate_keystrokes("end x enter");
    draw(cx);
    assert_eq!(
        turns_sent(&host),
        [("thread-a".to_owned(), "parent draftx".to_owned())],
        "the lead's draft is intact"
    );
}

/// A desktop pane whose field has the keyboard turning to a subagent's
/// thread: the keyboard stays in the pane, on the settings row, where a
/// key types nothing and sends nothing; back on the lead, the field has
/// it again and types.
#[gpui::test]
fn a_subagent_thread_keeps_the_keyboard_in_its_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::test_client::init);
    let (shell, host, _, cx) =
        mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 1200., "plan");
    restore_index(&shell, &host, true, cx);
    restore_status(&shell, &host, cx);
    deliver_thread(&shell, &host, "thread-a", false, cx);
    crate::test_frames::draw_settled(cx);
    let card = cx.debug_bounds("composer-card").expect("the lead's card");
    cx.simulate_click(
        gpui::point(card.center().x, card.top() + px(16.)),
        gpui::Modifiers::default(),
    );
    draw(cx);
    assert_eq!(
        typed_reaches(&host, cx),
        ["thread-a"],
        "the lead's field types"
    );

    deliver_thread(&shell, &host, "child", true, cx);
    crate::test_frames::draw_settled(cx);
    sent(&host);
    assert_eq!(
        keyboard_pane(&shell, cx),
        Some(0),
        "the keyboard left the pane"
    );
    cx.simulate_keystrokes("x y enter");
    draw(cx);
    assert!(
        sent(&host)
            .iter()
            .all(|payload| !matches!(payload, ClientPayload::Command(_))),
        "a key on a subagent's thread sent a command"
    );

    deliver_thread(&shell, &host, "thread-a", false, cx);
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        typed_reaches(&host, cx),
        ["thread-a"],
        "the field has it back, with nothing typed on the subagent's thread"
    );
}

#[gpui::test]
fn software_keyboard_restored_terminal_does_not_raise_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::window_seam::force_mobile_layout);
    let (shell, host, _, cx) = mount_restored_at_width(
        cx,
        &["hosts", "threads", "thread", "panel"],
        true,
        1024.,
        "terminal",
    );
    restore_index(&shell, &host, true, cx);
    let mut status = session_status("thread-a", std::path::Path::new("/project"));
    status.terminal_open = true;
    host.incoming
        .try_send(
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: "thread-a".into(),
                },
                event: ServerEvent::SessionStatusReplaced(Box::new(status)),
            }))
            .unwrap(),
        )
        .unwrap();
    await_restore_update(&shell, cx, |store, cx| {
        store
            .chat_active_session_for(Some("thread-a"), cx)
            .is_some()
    });
    host.incoming
        .try_send(
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::Settings,
                event: ServerEvent::SettingsSnapshot(Default::default()),
            }))
            .unwrap(),
        )
        .unwrap();
    host.incoming
        .try_send(
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::SessionEvents {
                    session_id: "thread-a".into(),
                },
                event: ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 0,
                    records: vec![],
                },
            }))
            .unwrap(),
        )
        .unwrap();
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-a"), cx)
    });
    assert!(store_of(&shell, cx).read_with(cx, |store, _| {
        store.panel_state_for(Some("thread-a")).terminal_open
    }));
    crate::test_frames::draw_settled(cx);
    cx.update(|window, cx| {
        assert!(
            window.focused(cx).is_none(),
            "a restored terminal must not steal focus after navigation has blurred"
        )
    });
}

#[gpui::test]
fn timeline_selection_copies_without_focusing_the_composer(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    cx.update(|window, _| window.activate_window());
    let session = "thread-1";
    let cwd = std::path::Path::new("/tmp/tcode-selection");
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
    let mut meta =
        tcode_core::project::SessionMeta::new(agent::ProviderKind::ClaudeCode, cwd.into(), None);
    meta.id = session.into();
    for (topic, event) in [
        (
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        ),
        (
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions: vec![meta],
                projects: Vec::new(),
            }),
        ),
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
                end: records.len() as u64,
                records: records.into_iter().map(Into::into).collect(),
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
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| store.show_thread(session.into(), cx));
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some(session), cx)
    });
    resize(cx, 1200.);
    let text = cx
        .debug_bounds("markdown-block-0")
        .expect("the answer's paragraph");
    let at = |dx: f32| gpui::point(text.left() + px(dx), text.center().y);
    let none = gpui::Modifiers::default();
    let select =
        |from: gpui::Point<Pixels>, to: gpui::Point<Pixels>, cx: &mut VisualTestContext| {
            cx.simulate_mouse_down(from, MouseButton::Left, none);
            draw(cx);
            for step in [from + gpui::point(px(5.), px(0.)), to] {
                cx.simulate_mouse_move(step, Some(MouseButton::Left), none);
                draw(cx);
            }
            cx.simulate_mouse_up(to, MouseButton::Left, none);
            draw(cx);
            cx.simulate_keystrokes("secondary-c");
            draw(cx);
            cx.read_from_clipboard().and_then(|item| item.text())
        };

    assert_eq!(
        select(at(1.), at(600.), cx).as_deref(),
        Some("Selectable answer alpha beta gamma")
    );
    assert_eq!(typed_reaches(&host, cx), Vec::<String>::new(), "selecting");

    cx.simulate_mouse_down(at(30.), MouseButton::Left, none);
    cx.simulate_mouse_up(at(30.), MouseButton::Left, none);
    draw(cx);
    assert_eq!(typed_reaches(&host, cx), Vec::<String>::new(), "a press");
    let card = cx.debug_bounds("composer-card").expect("the composer");
    cx.simulate_click(card.center(), none);
    assert_eq!(
        typed_reaches(&host, cx),
        [session],
        "a press on the composer"
    );
}

/// Text being selected holds still under the pointer while a reply
/// streams in below it: the transcript stops following the tail for as
/// long as the button is held, so what the drag ends on is what the
/// pointer is over, not what has scrolled under it since the press.
#[gpui::test]
fn a_drag_in_the_transcript_selects_under_the_pointer_while_a_reply_streams(
    cx: &mut TestAppContext,
) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    // The streamed text lands at once, and the tail follows it there.
    cx.update(|window, cx| {
        window.activate_window();
        cx.set_reduce_motion(true);
    });
    let session = "thread-1";
    let cwd = std::path::Path::new("/tmp/tcode-selection");
    let answer = |id: &str, text: String| {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: agent::ItemContent::AssistantMessage {
                text,
                summarized: false,
            },
        })
    };
    let streamed = |id: &str, text: String| agent::AgentEvent::Delta {
        item_id: id.into(),
        kind: agent::DeltaKind::AssistantText,
        text,
    };
    let records: Vec<agent::AgentEvent> = vec![
        agent::AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        answer(
            "finished",
            format!(
                "Done.\n\nselectable {}",
                "words of the finished answer ".repeat(12)
            ),
        ),
        agent::AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: agent::TurnStatus::Completed,
            usage: None,
        },
        agent::AgentEvent::TurnStarted {
            turn_id: "turn-2".into(),
        },
        streamed("streaming", "The next answer arrives. ".repeat(20)),
    ];
    let mut meta =
        tcode_core::project::SessionMeta::new(agent::ProviderKind::ClaudeCode, cwd.into(), None);
    meta.id = session.into();
    let mut status = session_status(session, cwd);
    status.activity.working = true;
    status.activity.turn_running = true;
    status.running_turn = Some(tcode_core::session::RunningTurn {
        turn: 1,
        started_at: Some(cx.update(|_, cx| crate::time::now_millis(cx))),
    });
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
                    sessions: vec![meta],
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::SessionStatus {
                    session_id: session.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            ),
            (
                Topic::SessionEvents {
                    session_id: session.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: records.len() as u64,
                    total_turns: 2,
                    truncated: false,
                    from: 0,
                    end: records.len() as u64,
                    records: records.into_iter().map(Into::into).collect(),
                },
            ),
        ],
    );
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| store.show_thread(session.into(), cx));
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some(session), cx)
    });
    cx.simulate_resize(size(px(1200.), px(560.)));
    draw(cx);
    // The finished answer's second paragraph, its first line.
    let text = cx
        .debug_bounds("markdown-block-1")
        .expect("the finished answer's paragraph");
    let line = text.top() + px(8.);
    let none = gpui::Modifiers::default();
    let (from, to) = (
        gpui::point(text.left() + px(2.), line),
        gpui::point(text.left() + px(70.), line),
    );
    // What a copy takes, or None when it takes nothing.
    let copy = |cx: &mut VisualTestContext| {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new()));
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-c"
        } else {
            "ctrl-c"
        });
        draw(cx);
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .filter(|text| !text.is_empty())
    };
    // The same drag with nothing arriving: what the pointer passes over.
    cx.simulate_mouse_down(from, MouseButton::Left, none);
    draw(cx);
    cx.simulate_mouse_move(to, Some(MouseButton::Left), none);
    draw(cx);
    cx.simulate_mouse_up(to, MouseButton::Left, none);
    draw(cx);
    let still = copy(cx);
    assert!(still.as_deref().is_some_and(|text| text.starts_with("sel")));

    cx.simulate_mouse_down(from, MouseButton::Left, none);
    draw(cx);
    for step in 1..=8 {
        // Lines of the streaming answer arrive with each frame.
        send_events(
            &host,
            vec![(
                Topic::SessionEvents {
                    session_id: session.into(),
                },
                ServerEvent::SessionEvent(
                    streamed(
                        "streaming",
                        format!("arrived {step} {}", "and more ".repeat(12)),
                    )
                    .into(),
                ),
            )],
        );
        cx.run_until_parked();
        cx.run_until_parked();
        let at = from + (to - from) * (step as f32 / 8.);
        cx.simulate_mouse_move(at, Some(MouseButton::Left), none);
        draw(cx);
    }
    cx.simulate_mouse_up(to, MouseButton::Left, none);
    draw(cx);
    assert_eq!(copy(cx), still);
}

#[gpui::test]
fn a_click_in_the_thread_pane_focuses_the_composer_only_on_desktop(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (_shell, host, cx) = mount_panes(cx);
    resize(cx, 1200.);
    let timeline = gpui::point(px(700.), px(300.));
    for mobile in [false, true] {
        cx.update(|window, cx| {
            if mobile {
                crate::window_seam::force_mobile_layout(cx);
            }
            window.blur(cx);
        });
        draw(cx);
        cx.simulate_mouse_down(timeline, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(timeline, MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
        let reached: &[&str] = if mobile { &[] } else { &["thread-1"] };
        assert_eq!(typed_reaches(&host, cx), reached, "mobile: {mobile}");
    }
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

/// A thread dragged from the threads list to a pane's right edge shows,
/// while it hovers, the half of the pane it would take, and opens there
/// with the keyboard on release.
#[gpui::test]
fn a_thread_dropped_on_a_panes_edge_opens_beside_it(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let pane = only_pane(&shell, cx);
    let to = gpui::point(pane.right() - px(40.), pane.center().y);
    drag_row("sidebar-thread-thread-2", to, cx);

    cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
    draw(cx);
    serve_thread(&host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(1));
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-2"));
        assert!(store.is_thread_open("thread-1"));
    });
}

/// Dropped in a pane's middle, a thread takes that pane's place and the
/// thread it showed closes.
#[gpui::test]
fn a_thread_dropped_on_a_panes_middle_replaces_its_thread(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let pane = only_pane(&shell, cx);
    drag_row("sidebar-thread-thread-2", pane.center(), cx);

    cx.simulate_mouse_up(pane.center(), MouseButton::Left, gpui::Modifiers::default());
    draw(cx);
    serve_thread(&host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-2"));
        assert!(!store.is_thread_open("thread-1"));
    });
}

/// A press on a subagent's window in its lead's timeline opens the
/// agent's thread beside the lead, in the lead's own pane, with the
/// keyboard; Escape from it closes it and gives the keyboard back to the
/// lead's composer; once shown, the same press leaves the one agent
/// shown rather than opening it twice.
#[gpui::test]
fn a_subagent_window_opens_its_thread_beside_its_lead(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    // The lead is reopened below, so the host answers it with the log
    // that spawned the agent.
    cx.simulate_keystrokes("secondary-2");
    serve_thread(&host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
        unreachable!("panes_index sends a snapshot");
    };
    let mut mirror = index.sessions[1].clone();
    mirror.id = "mirror".into();
    mirror.title = "mirror".into();
    mirror.parent_session_id = Some("thread-1".into());
    mirror.native_subagent = Some("spawn-1".into());
    index.sessions.push(mirror);
    let spawn = agent::AgentEvent::ItemStarted(agent::ThreadItem {
        id: "spawn-1".into(),
        parent_item_id: None,
        content: agent::ItemContent::Subagent {
            agent_type: "explore".into(),
            description: "Survey the panes".into(),
            status: agent::ItemStatus::InProgress,
            summary: None,
            model: None,
            effort: None,
        },
    });
    let records: Vec<_> = [
        agent::AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: "ask".into(),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: "Look around".into(),
                attachments: vec![],
            },
        }),
        spawn,
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    send_events(
        &host,
        vec![
            (topic, ServerEvent::IndexSnapshot(index)),
            (
                Topic::SessionEvents {
                    session_id: "thread-1".into(),
                },
                ServerEvent::SessionSnapshot {
                    total: records.len() as u64,
                    total_turns: 1,
                    truncated: false,
                    from: 0,
                    end: records.len() as u64,
                    records,
                },
            ),
        ],
    );
    cx.simulate_keystrokes("secondary-1");
    await_restore_update(&shell, cx, |store, cx| {
        store.thread_meta("mirror").is_some() && !store.chat_loading_for(Some("thread-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    let log = cx
        .debug_bounds("worklog-header-0-spawn-1")
        .expect("the turn's work log");
    cx.simulate_mouse_move(log.center(), None, gpui::Modifiers::default());
    draw(cx);
    cx.simulate_click(log.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    let window = cx
        .debug_bounds("activity-row-spawn-1")
        .expect("the subagent's window");
    cx.simulate_click(window.center(), gpui::Modifiers::default());
    draw(cx);
    serve_thread(&host, "mirror");
    // The host makes a native subagent's thread read-only.
    let mut status = session_status("mirror", std::path::Path::new(PANES_CWD));
    status.conversation_read_only = true;
    send_events(
        &host,
        vec![(
            Topic::SessionStatus {
                session_id: "mirror".into(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        )],
    );
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("mirror"), cx)
            && store.native_subagent_readonly_for(Some("mirror"), cx)
    });
    crate::test_frames::draw_settled(cx);
    let store = store_of(&shell, cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    assert!(cx.debug_bounds("chat-side-header").is_some());
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    store.read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-1"));
        assert!(store.is_thread_open("mirror"));
    });
    // The keyboard is with the agent, whose composer is its settings
    // row, which has no field: a key types nowhere.
    assert!(cx.debug_bounds("composer-subagent").is_some());
    sent(&host);
    cx.simulate_keystrokes("x enter");
    draw(cx);
    assert!(
        sent(&host)
            .iter()
            .all(|payload| !matches!(payload, ClientPayload::Command(_)))
    );

    cx.simulate_keystrokes("escape");
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("chat-side-header").is_none());
    assert_eq!(
        typed_reaches(&host, cx),
        ["thread-1"],
        "the keyboard is back in the lead's composer"
    );
    store.read_with(cx, |store, _| assert!(!store.is_thread_open("mirror")));

    for press in 0..2 {
        let window = cx
            .debug_bounds("activity-row-spawn-1")
            .expect("the subagent's window");
        cx.simulate_click(window.center(), gpui::Modifiers::default());
        crate::test_frames::draw_settled(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into())],
            "press {press}"
        );
        assert!(
            cx.debug_bounds("chat-side-header").is_some(),
            "press {press}"
        );
        assert_eq!(
            typed_reaches(&host, cx),
            Vec::<String>::new(),
            "press {press}"
        );
        store.read_with(cx, |store, _| assert!(store.is_thread_open("mirror")));
    }
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

/// An agent opened from its lead's Agents panel shows beside the lead in
/// the lead's own pane, the panel staying open beside them, and the lead
/// keeping the keyboard's pane. Close takes it away, the panel still
/// there, and the agent's thread closes.
#[gpui::test]
fn an_agent_opens_beside_its_lead_in_the_leads_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Agents, cx)
    });
    crate::test_frames::draw_settled(cx);
    open_agent_from_panel(&shell, &host, cx);
    assert!(
        cx.debug_bounds("right-panel").is_some(),
        "the panel stays beside the agent"
    );
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    assert!(
        cx.debug_bounds("chat-side-header").is_some(),
        "the agent beside its lead"
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    store.read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-1"));
        assert!(store.is_thread_open("agent-1"));
    });

    press_control("side-close", cx);
    assert!(cx.debug_bounds("chat-side-header").is_none());
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    assert!(cx.debug_bounds("right-panel").is_some(), "the panel stays");
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    store.read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-1"));
        assert!(!store.is_thread_open("agent-1"));
    });
}

/// On a tablet: an agent beside its lead is neither drawn nor held open
/// while the window is compact, which shows the lead alone, and is beside
/// its lead again once the window is wide. An agent opened from its lead
/// in the compact window takes the lead's place, and the compact Back
/// returns to the lead and closes the agent.
#[gpui::test]
fn an_agent_in_a_compact_window_takes_its_leads_place(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    as_mobile(cx);
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Agents, cx)
    });
    crate::test_frames::draw_settled(cx);
    open_agent_from_panel(&shell, &host, cx);
    assert!(cx.debug_bounds("chat-side-header").is_some());

    resize(cx, 600.);
    crate::test_frames::draw_settled(cx);
    assert!(shell.read_with(cx, |shell, cx| shell.compact(cx)));
    assert!(cx.debug_bounds("chat-side-header").is_none());
    store.read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-1"));
        assert!(!store.is_thread_open("agent-1"));
    });
    resize(cx, 2000.);
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    assert!(cx.debug_bounds("chat-side-header").is_some());
    store.read_with(cx, |store, _| assert!(store.is_thread_open("agent-1")));
    press_control("side-close", cx);

    resize(cx, 600.);
    crate::test_frames::draw_settled(cx);
    cx.update(|window, cx| {
        crate::agents_panel::open_agent(
            &store,
            "agent-1".into(),
            crate::shell::side_host_for(&store.read(cx).thread_key("thread-1"), cx),
            None,
            window,
            cx,
        )
    });
    draw(cx);
    serve_thread(&host, "agent-1");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("agent-1"), cx) && store.timeline_settled_for("agent-1")
    });
    crate::test_frames::draw_settled(cx);
    store.read_with(cx, |store, _| {
        assert!(store.is_thread_open("agent-1"));
    });
    assert!(cx.debug_bounds("chat-side-header").is_none());

    let back = cx.debug_bounds("compact-back").expect("the compact Back");
    cx.simulate_click(back.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    store.read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-1"));
        assert!(!store.is_thread_open("agent-1"));
    });
    assert!(cx.debug_bounds("compact-thread-page").is_some());
}

/// On a phone, the Agents pill under a lead's timeline opens a sheet
/// that shows the lead's agents.
#[gpui::test]
fn the_phone_agents_sheet_shows_the_leads_agents(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (_shell, _host, cx) = mount_lead_with_agent(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    as_mobile(cx);
    resize(cx, 393.);
    crate::test_frames::draw_settled(cx);
    let pill = cx.debug_bounds("agents-pill").expect("the Agents pill");
    cx.simulate_click(pill.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert!(
        cx.debug_bounds("touch-picker-content").is_some(),
        "the Agents sheet"
    );
    assert!(
        cx.debug_bounds("agent-row-agent-1").is_some(),
        "the agent's row"
    );
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

/// Two columns from one pane keep its thread on the left and leave the
/// right waiting; a pane command from the palette hands the keyboard back
/// to the focused pane's composer; a single pane
/// from two keeps the focused thread and closes the other; ⌥⌘2 lays out
/// two columns again. Each layout goes in as its fade plays.
#[gpui::test]
fn a_layout_fills_its_panes_from_the_focused_one(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, host, cx) = mount_panes(cx);
    choose_from_palette(&shell, &PaneLayout::Columns.label(), cx);
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
    assert!(cx.debug_bounds("waiting-pane").is_some());
    assert_eq!(keyboard_pane(&shell, cx), Some(0));

    cx.simulate_keystrokes("secondary-alt-right secondary-2");
    serve_thread(&host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    for command in [crate::tr!("panes.zoom"), crate::tr!("panes.unzoom")] {
        choose_from_palette(&shell, &command, cx);
        crate::test_frames::draw_settled(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(1), "{command}");
    }
    assert_eq!(typed_reaches(&host, cx), ["thread-2"], "the caret is back");

    choose_from_palette(&shell, &PaneLayout::Single.label(), cx);
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-2"));
        assert!(!store.is_thread_open("thread-1"));
    });

    cx.simulate_keystrokes("secondary-alt-2");
    draw(cx);
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into()), None]);
}

/// A row of a pane header's menu leaves the keyboard in that pane's
/// composer, as a pane command from the palette does, not on the menu's
/// button.
#[gpui::test]
fn a_pane_menu_row_hands_the_keyboard_to_the_composer(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
    let button = cx.debug_bounds("pane-menu").expect("the pane menu");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    draw(cx);
    let pane = keyboard_pane(&shell, cx).expect("the menu's pane has the keyboard");
    // Split right, split down, a separator, then zoom.
    let zoom = cx.debug_bounds("menu-item-3").expect("the zoom row");
    cx.simulate_click(zoom.center(), gpui::Modifiers::default());
    draw(cx);
    let zoomed = shell.read_with(cx, |shell, _| {
        shell.column.as_ref().expect("attached").zoom.is_zoomed()
    });
    assert!(zoomed, "the row ran");
    assert_eq!(keyboard_pane(&shell, cx), Some(pane));
    let thread = pane_threads(&shell, cx)[pane].clone().expect("a thread");
    assert_eq!(typed_reaches(&host, cx), [thread], "the caret is back");
}

/// A thread's agents spread beside it newest first, stacked on the
/// right; one that has settled stays out.
#[gpui::test]
fn spreading_agents_leaves_settled_ones_out(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
        unreachable!("panes_index sends a snapshot");
    };
    for (id, created_at, settled_at) in [
        ("agent-old", 1, None),
        ("agent-new", 2, None),
        ("agent-settled", 3, Some(4)),
    ] {
        let mut agent = index.sessions[1].clone();
        agent.id = id.into();
        agent.title = id.into();
        agent.parent_session_id = Some("thread-1".into());
        agent.created_at = created_at;
        agent.settled_at = settled_at;
        index.sessions.push(agent);
    }
    send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
    await_restore_update(&shell, cx, |store, _| {
        store.thread_meta("agent-settled").is_some()
    });
    draw(cx);

    choose_from_palette(&shell, &PaneLayout::SpreadAgents.label(), cx);
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [
            Some("thread-1".into()),
            Some("agent-new".into()),
            Some("agent-old".into())
        ]
    );
}

/// Tab and ⇧Tab in a zoomed pane keep the keyboard in it: the panes it
/// covers are still laid out, tab stops and all, but never take it. Back
/// in its slot the pane keeps the keyboard, and Tab leaves it again.
#[gpui::test]
fn tabbing_in_a_zoomed_pane_never_reaches_the_panes_it_covers(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    cx.simulate_keystrokes("secondary-d");
    crate::test_frames::draw_settled(cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(1));
    cx.simulate_keystrokes("secondary-shift-enter");
    crate::test_frames::draw_settled(cx);
    for key in ["tab", "shift-tab"] {
        for press in 0..30 {
            cx.simulate_keystrokes(key);
            draw(cx);
            assert_eq!(keyboard_pane(&shell, cx), Some(1), "{key} #{press}");
        }
    }

    cx.simulate_keystrokes("secondary-shift-enter");
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("zoomed-pane").is_none());
    assert_eq!(
        keyboard_pane(&shell, cx),
        Some(1),
        "still in the pane, now in its slot"
    );
    let reached = (0..60).any(|_| {
        cx.simulate_keystrokes("tab");
        draw(cx);
        keyboard_pane(&shell, cx) == Some(0)
    });
    assert!(
        reached,
        "back in its slot, Tab reaches the other pane again"
    );
}

/// Splitting a thread puts a draft beside it, in the thread's place in
/// the store's focus, while the thread stays open in its own pane.
#[gpui::test]
fn splitting_a_thread_shows_a_draft_beside_it(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    draw(cx);
    let _ = sent(&host);

    cx.simulate_keystrokes("secondary-d");
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
    assert!(cx.debug_bounds("waiting-pane").is_some());
    answer_draft(&shell, &host, "draft-1", cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(1));
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("waiting-pane").is_none());
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("draft-1".into())]
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(1));
    store.read_with(cx, |store, _| {
        assert!(store.is_thread_open("draft-1"));
        assert!(store.is_thread_open("thread-1"));
    });

    cx.simulate_keystrokes("secondary-alt-left");
    draw(cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell
            .page_thread(cx)
            .map(|thread| thread.session_id)),
        Some("thread-1".into())
    );
}

/// The keyboard leaving a new pane before its draft is answered drops the
/// draft: the user has moved on, and it would land in a pane left behind.
#[gpui::test]
fn leaving_a_new_pane_drops_its_draft(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.simulate_keystrokes("secondary-d");
    draw(cx);
    cx.simulate_keystrokes("secondary-alt-left");
    draw(cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));

    ack_draft(&host, "draft-1");
    cx.run_until_parked();
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
}

/// Beside a draft no draft is asked for, since the host would answer with
/// that same draft: the new pane waits with the keyboard, and the thread
/// chosen next from the threads list fills it.
#[gpui::test]
fn splitting_a_draft_leaves_the_new_pane_waiting(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_status(&host, "thread-1", true);
    await_restore_update(&shell, cx, |store, cx| {
        store
            .session_status_for(Some("thread-1"), cx)
            .is_some_and(|status| status.draft)
    });
    draw(cx);
    let _ = sent(&host);

    cx.simulate_keystrokes("secondary-d");
    draw(cx);
    assert!(!sent(&host).iter().any(|payload| matches!(
        payload,
        ClientPayload::Command(Command::StartDraft { .. } | Command::StartScratchDraft)
    )));
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
    assert_eq!(keyboard_pane(&shell, cx), Some(1));
    assert!(cx.debug_bounds("waiting-pane").is_some());
    crate::test_frames::draw_settled(cx);

    cx.simulate_keystrokes("secondary-2");
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-2"));
        assert!(store.is_thread_open("thread-1"));
    });
}

/// A pane waiting for a thread has no chat header, so no pane menu: its
/// corner button closes it, as ⇧⌘W and Escape do with the keyboard in it.
#[gpui::test]
fn a_waiting_pane_closes_from_its_corner_and_the_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    serve_status(&host, "thread-1", true);
    await_restore_update(&shell, cx, |store, cx| {
        store
            .session_status_for(Some("thread-1"), cx)
            .is_some_and(|status| status.draft)
    });
    draw(cx);
    let split = |cx: &mut VisualTestContext| {
        cx.simulate_keystrokes("secondary-d");
        crate::test_frames::draw_settled(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
    };
    let closed = |cx: &mut VisualTestContext, how: &str| {
        crate::test_frames::draw_settled(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())], "{how}");
    };

    split(cx);
    let close = cx
        .debug_bounds("waiting-pane-close")
        .expect("the waiting pane's close button");
    cx.simulate_click(close.center(), gpui::Modifiers::default());
    draw(cx);
    closed(cx, "corner button");
    assert!(cx.debug_bounds("waiting-pane-close").is_none());

    split(cx);
    cx.simulate_keystrokes("secondary-shift-w");
    draw(cx);
    closed(cx, "secondary-shift-w");

    split(cx);
    cx.simulate_keystrokes("escape");
    draw(cx);
    closed(cx, "escape");
}

/// Three panes, the keyboard in the top right one: ⌥⌘W glides the other
/// two shut while the keyboard stays, and their threads close, still
/// listed.
#[gpui::test]
fn closing_other_panes_leaves_the_focused_one(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
    cx.simulate_keystrokes("secondary-shift-d");
    draw(cx);
    answer_draft(&shell, &host, "draft-1", cx);
    crate::test_frames::draw_settled(cx);
    cx.simulate_keystrokes("secondary-alt-up");
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [
            Some("thread-1".into()),
            Some("thread-2".into()),
            Some("draft-1".into())
        ]
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(1));

    cx.simulate_keystrokes("secondary-alt-w");
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-2"));
        assert!(!store.is_thread_open("thread-1"));
        assert!(!store.is_thread_open("draft-1"));
    });
    assert!(cx.debug_bounds("sidebar-thread-thread-1").is_some());
}

/// A thread the client still holds, opened again in a pane, waits on its
/// skeleton for the host to catch it up, drawn unseen beneath it. A
/// click in that pane gives it the keyboard, and what is typed once the
/// host has answered goes there.
#[gpui::test]
fn a_click_on_a_pane_catching_up_gives_it_the_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    let click = |at: gpui::Point<Pixels>, modifiers, cx: &mut VisualTestContext| {
        cx.simulate_mouse_move(at, None, modifiers);
        draw(cx);
        cx.simulate_click(at, modifiers);
        crate::test_frames::draw_settled(cx);
    };
    let row = |id: &'static str, cx: &mut VisualTestContext| {
        cx.debug_bounds(id).expect("the thread's row").center()
    };
    // thread-2 in thread-1's place, so thread-1 is left and kept.
    click(row("sidebar-thread-thread-2", cx), Default::default(), cx);
    serve_thread(&host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    crate::test_frames::draw_settled(cx);
    let alt = gpui::Modifiers {
        alt: true,
        ..Default::default()
    };
    click(row("sidebar-thread-thread-1", cx), alt, cx);
    cx.simulate_keystrokes("secondary-alt-left");
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-2".into()), Some("thread-1".into())]
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    store.read_with(cx, |store, cx| {
        assert!(!store.chat_loading_for(Some("thread-1"), cx));
        assert!(!store.timeline_settled_for("thread-1"), "it is catching up");
    });

    let right = pane_bounds(&shell, cx)[1];
    click(right.center(), Default::default(), cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(1));
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell
            .page_thread(cx)
            .map(|thread| thread.session_id)),
        Some("thread-1".into())
    );

    serve_thread(&host, "thread-1");
    await_restore_update(&shell, cx, |store, _| {
        store.timeline_settled_for("thread-1")
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(typed_reaches(&host, cx), ["thread-1"]);
}

/// A click in a pane whose thread is still loading gives it the keyboard,
/// so what is typed next goes there once it has loaded, not to the pane
/// that had the keyboard before.
#[gpui::test]
fn a_click_on_a_loading_pane_gives_it_the_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
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
    cx.simulate_keystrokes("secondary-alt-left");
    crate::test_frames::draw_settled(cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    assert!(
        store.read_with(cx, |store, cx| store.chat_loading_for(Some("thread-2"), cx)),
        "the right pane's thread has not been answered"
    );

    let right = pane_bounds(&shell, cx)[1];
    cx.simulate_mouse_move(right.center(), None, gpui::Modifiers::default());
    draw(cx);
    cx.simulate_click(right.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(1));
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell
            .page_thread(cx)
            .map(|thread| thread.session_id)),
        Some("thread-2".into())
    );

    serve_thread(&host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(typed_reaches(&host, cx), ["thread-2"]);
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

/// A panel toggle in the header of the pane without the keyboard acts on
/// that pane's thread. Neither the press nor the release moves the
/// keyboard; the release opens the thread's panel docked in its own pane.
/// Each toggle closes only the panel of its own thread.
// Finds the left pane's toggle at the right pane's distance from its
// pane's end. On Windows the rightmost header ends in the window's
// caption buttons and the left one does not, so there the point is not
// the toggle and this drives nothing a user does.
#[cfg(not(target_os = "windows"))]
#[gpui::test]
fn a_header_toggle_acts_on_the_thread_of_its_own_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    side_by_side(
        &shell,
        || serve_spawning(&host, "thread-2", &["spawn-1"]),
        cx,
    );
    let bounds = |selector: &'static str, cx: &mut VisualTestContext| {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("no {selector}"))
    };

    let agents = bounds("agents-panel", cx);
    assert!(pane_bounds(&shell, cx)[1].contains(&agents.center()));
    cx.simulate_click(agents.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert_eq!(panel_of(&store, "thread-2", cx), Some(RightTab::Agents));
    assert_eq!(panel_of(&store, "thread-1", cx), None);
    assert_eq!(keyboard_pane(&shell, cx), Some(0), "the panel leaves it");
    assert!(
        cx.debug_bounds("agent-row-spawn-1").is_some(),
        "the panel lists the agents of the thread whose toggle was pressed"
    );

    let close = bounds("diff-close", cx);
    cx.simulate_click(close.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    // The header takes its wide tiers again as the conversation widens
    // after the panel, over the frames a display goes on delivering.
    crate::test_frames::draw_settled(cx);
    assert_eq!(panel_of(&store, "thread-2", cx), None);
    assert_eq!(keyboard_pane(&shell, cx), Some(0), "still where it was");

    let diff = bounds("diff-panel", cx);
    press_with_a_frame_between(diff.center(), cx);
    assert_eq!(panel_of(&store, "thread-2", cx), Some(RightTab::Diff));
    assert_eq!(panel_of(&store, "thread-1", cx), None);

    // The left pane's toggle, the same distance from its pane's end,
    // opens the left thread's drawer and leaves the right thread's.
    let panes = pane_bounds(&shell, cx);
    let at = gpui::point(
        panes[0].right() - (panes[1].right() - diff.center().x),
        diff.center().y,
    );
    press_with_a_frame_between(at, cx);
    assert_eq!(panel_of(&store, "thread-1", cx), Some(RightTab::Diff));
    assert_eq!(panel_of(&store, "thread-2", cx), Some(RightTab::Diff));
}

/// A file in the card of an agent beside its lead opens the agent's diff
/// in the drawer of the lead's pane, which says whose it is, and the
/// agent stays beside its lead. The drawer closed, the lead's header's
/// Diff toggle shows the lead's diff. The agent's card closing takes its
/// diff with it, and the lead's panel does not take its place.
#[gpui::test]
fn a_changed_file_in_an_agents_card_opens_the_agents_diff(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    serve_changed_turn(&host, "thread-1", "lead.rs");
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx) == Some(1)
    });
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", RightTab::Agents, cx)
    });
    crate::test_frames::draw_settled(cx);
    open_agent_from_panel(&shell, &host, cx);
    serve_changed_turn(&host, "agent-1", "agent.rs");
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("agent-1"), |timeline| timeline.turns.len(), cx) == Some(1)
    });
    crate::test_frames::draw_settled(cx);

    let shows_agents_file = |cx: &mut VisualTestContext| {
        press_control("changed-file-chip-file-agent.rs", cx);
        draw_until(cx, &host, "diff-line-new-agent.rs-1", None);
        assert_eq!(cx.debug_bounds("diff-line-new-lead.rs-1"), None);
        assert!(cx.debug_bounds("diff-subject").is_some());
        assert_eq!(panel_of(&store, "agent-1", cx), Some(RightTab::Diff));
    };
    let shows_leads_diff = |cx: &mut VisualTestContext| {
        draw_until(cx, &host, "diff-line-new-lead.rs-1", None);
        assert_eq!(cx.debug_bounds("diff-line-new-agent.rs-1"), None);
        assert_eq!(cx.debug_bounds("diff-subject"), None);
    };

    shows_agents_file(cx);
    press_control("diff-close", cx);
    assert_eq!(panel_of(&store, "agent-1", cx), None);
    assert_eq!(cx.debug_bounds("right-panel"), None);
    assert!(cx.debug_bounds("chat-side-header").is_some(), "it stays");

    shows_agents_file(cx);
    press_control("side-close", cx);
    assert_eq!(cx.debug_bounds("chat-side-header"), None);
    assert_eq!(cx.debug_bounds("right-panel"), None, "nothing in its place");
    assert_eq!(panel_of(&store, "thread-1", cx), None);
    press_control("diff-panel", cx);
    shows_leads_diff(cx);
}

/// On a compact agent's page a changed file opens the agent's diff on the
/// panel page over it: there is no pane beside a lead to show it in, and
/// the page's chat is the agent's.
#[gpui::test]
fn a_changed_file_on_a_compact_agents_page_opens_the_agents_diff(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    as_mobile(cx);
    let store = store_of(&shell, cx);
    serve_changed_turn(&host, "thread-1", "lead.rs");
    resize(cx, 600.);
    crate::test_frames::draw_settled(cx);
    cx.update(|window, cx| {
        crate::agents_panel::open_agent(&store, "agent-1".into(), None, None, window, cx)
    });
    draw(cx);
    serve_thread(&host, "agent-1");
    serve_changed_turn(&host, "agent-1", "agent.rs");
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("agent-1"), |timeline| timeline.turns.len(), cx) == Some(1)
    });
    crate::test_frames::draw_settled(cx);

    press_control("changed-file-chip-file-agent.rs", cx);
    draw_until(cx, &host, "diff-line-new-agent.rs-1", None);
    assert_eq!(cx.debug_bounds("diff-line-new-lead.rs-1"), None);
    assert_eq!(panel_of(&store, "agent-1", cx), Some(RightTab::Diff));
    assert_eq!(panel_of(&store, "thread-1", cx), None);
}

/// Two panes each have a panel. Diff opened from the left pane's header
/// opens inside that pane and the right pane's thread keeps its own shut;
/// Plan opened in the right pane opens there, each with its own thread's
/// content. A thread moved to the other pane takes its panel along.
// Finds the left pane's toggle at the right pane's distance from its
// pane's end. On Windows the rightmost header ends in the window's
// caption buttons and the left one does not, so there the point is not
// the toggle and this drives nothing a user does.
#[cfg(not(target_os = "windows"))]
#[gpui::test]
fn each_pane_has_a_drawer_of_its_own(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    serve_changed_turn(&host, "thread-1", "left.rs");
    side_by_side(
        &shell,
        || {
            serve_thread(&host, "thread-2");
            send_events(
                &host,
                vec![(
                    Topic::SessionPlan {
                        session_id: "thread-2".into(),
                    },
                    ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                        session_id: "thread-2".into(),
                        steps: vec![agent::PlanStep {
                            step: "Right".into(),
                            status: agent::PlanStepStatus::Pending,
                        }],
                        explanation: None,
                    }),
                )],
            );
        },
        cx,
    );
    crate::test_frames::draw_settled(cx);
    let panes = pane_bounds(&shell, cx);
    let inside = |pane: gpui::Bounds<Pixels>, bounds: gpui::Bounds<Pixels>| {
        pane.left() <= bounds.left()
            && bounds.right() <= pane.right()
            && pane.top() <= bounds.top()
            && bounds.bottom() <= pane.bottom()
    };

    let right_diff = cx.debug_bounds("diff-panel").expect("the Diff toggle");
    assert!(panes[1].contains(&right_diff.center()));
    let left_diff = gpui::point(
        panes[0].right() - (panes[1].right() - right_diff.center().x),
        right_diff.center().y,
    );
    cx.simulate_mouse_move(left_diff, None, gpui::Modifiers::default());
    draw(cx);
    cx.simulate_click(left_diff, gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    draw_until(cx, &host, "diff-line-new-left.rs-1", None);
    let drawer = cx.debug_bounds("right-panel").expect("the left drawer");
    assert!(inside(panes[0], drawer), "{drawer:?} in {panes:?}");
    assert_eq!(panel_of(&store, "thread-2", cx), None);

    // Uncovered, the right pane's own header still takes a click.
    let plan = cx.debug_bounds("plan-panel").expect("the Plan toggle");
    assert!(panes[1].contains(&plan.center()));
    cx.simulate_mouse_move(plan.center(), None, gpui::Modifiers::default());
    draw(cx);
    cx.simulate_click(plan.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert_eq!(panel_of(&store, "thread-1", cx), Some(RightTab::Diff));
    assert_eq!(panel_of(&store, "thread-2", cx), Some(RightTab::Plan));
    let diff = cx
        .debug_bounds("diff-line-new-left.rs-1")
        .expect("the left thread's diff");
    let step = cx
        .debug_bounds("plan-step-0:Right")
        .expect("the right thread's plan");
    assert!(inside(panes[0], diff), "{diff:?} in {panes:?}");
    assert!(inside(panes[1], step), "{step:?} in {panes:?}");

    // The keyboard stayed in the left pane, which carries its thread over.
    cx.simulate_keystrokes("secondary-alt-shift-right");
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-2".into()), Some("thread-1".into())]
    );
    draw_until(cx, &host, "diff-line-new-left.rs-1", None);
    let diff = cx
        .debug_bounds("diff-line-new-left.rs-1")
        .expect("the left thread's diff");
    let step = cx
        .debug_bounds("plan-step-0:Right")
        .expect("the right thread's plan");
    assert!(inside(panes[1], diff), "{diff:?} in {panes:?}");
    assert!(inside(panes[0], step), "{step:?} in {panes:?}");
}

/// The drawer's width is one preference: dragged in one thread's drawer,
/// it is the width another thread's drawer opens at, in another pane too.
#[gpui::test]
fn a_width_dragged_in_one_drawer_is_every_drawers(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    side_by_side(&shell, || serve_thread(&host, "thread-2"), cx);
    resize(cx, 2600.);
    crate::test_frames::draw_settled(cx);
    let open = |thread: &str, cx: &mut VisualTestContext| {
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for(thread, RightTab::Plan, cx)
        });
        crate::test_frames::draw_settled(cx);
        cx.debug_bounds("right-panel").expect("the drawer")
    };
    let first = open("thread-1", cx);

    // As a pointer does: the drag starts on the first move and moves the
    // handle on the next, each with its frame.
    let from = gpui::point(first.left(), px(400.));
    cx.simulate_mouse_move(from, None, gpui::Modifiers::default());
    draw(cx);
    cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
    let to = from + gpui::point(px(60.), px(0.));
    for step in [from + gpui::point(px(5.), px(0.)), to] {
        cx.simulate_mouse_move(step, Some(MouseButton::Left), gpui::Modifiers::default());
        draw(cx);
    }
    cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    let chosen = cx
        .debug_bounds("right-panel")
        .expect("the drawer")
        .size
        .width;
    assert!(chosen < first.size.width, "the edge shrinks the panel");

    store.update(cx, |store, cx| {
        store.close_right_panel_for(Some("thread-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    let other = open("thread-2", cx);
    assert_eq!(other.size.width, chosen, "the other pane's drawer");
}

/// A file listed in the rail of the pane without the keyboard opens that
/// thread's diff, also on a tap released before the next frame: the pane
/// hears the keyboard enter it only once a frame is drawn, so the store
/// has not moved yet.
#[gpui::test]
fn a_changed_file_opens_the_diff_of_the_thread_of_its_own_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    resize(cx, 3200.);
    side_by_side(
        &shell,
        || {
            let records: Vec<tcode_core::session::StoredEvent> = [
                agent::AgentEvent::TurnStarted {
                    turn_id: "turn-1".into(),
                },
                agent::AgentEvent::TurnChangesUpdated {
                    turn_id: "turn-1".into(),
                    changes: agent::file_changes_from_unified_diff(
                        "diff --git a/src/lib.rs b/src/lib.rs\n\
                             --- a/src/lib.rs\n+++ b/src/lib.rs\n\
                             @@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n",
                    )
                    .changes,
                    completeness: agent::ChangeCompleteness::Exact,
                },
                agent::AgentEvent::TurnCompleted {
                    turn_id: "turn-1".into(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                },
            ]
            .into_iter()
            .map(Into::into)
            .collect();
            serve_status(&host, "thread-2", false);
            send_events(
                &host,
                vec![(
                    Topic::SessionEvents {
                        session_id: "thread-2".into(),
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
        },
        cx,
    );
    crate::test_frames::draw_settled(cx);
    let row = cx
        .debug_bounds("changed-file-row-file-src/lib.rs")
        .expect("the file's row in the rail");
    assert!(pane_bounds(&shell, cx)[1].contains(&row.center()));

    cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::default());
    draw(cx);
    let press = gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: row.center(),
        modifiers: gpui::Modifiers::default(),
        click_count: 1,
        first_mouse: false,
    };
    let release = gpui::MouseUpEvent {
        button: MouseButton::Left,
        position: row.center(),
        modifiers: gpui::Modifiers::default(),
        click_count: 1,
    };
    cx.update(|window, cx| {
        window.dispatch_event(gpui::PlatformInput::MouseDown(press), cx);
        window.dispatch_event(gpui::PlatformInput::MouseUp(release), cx);
    });
    store.read_with(cx, |store, cx| {
        assert!(store.is_thread_open("thread-2"));
        assert_eq!(store.diff_selected_turn_for(Some("thread-2"), cx), Some(0));
    });
    assert_eq!(panel_of(&store, "thread-2", cx), Some(RightTab::Diff));
    assert_eq!(panel_of(&store, "thread-1", cx), None);
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        keyboard_pane(&shell, cx),
        Some(0),
        "the keyboard stays where it was, not lost with the row"
    );
}

/// The way back to an agent's lead in the header of the pane without the
/// keyboard shows the lead in that pane, not in the one with the keyboard.
#[gpui::test]
fn the_way_back_to_a_lead_stays_in_its_own_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
        unreachable!("panes_index sends a snapshot");
    };
    let mut agent = index.sessions[1].clone();
    agent.id = "agent-1".into();
    agent.title = "agent-1".into();
    agent.parent_session_id = Some("thread-2".into());
    index.sessions.push(agent);
    send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
    await_restore_update(&shell, cx, |store, _| {
        store.thread_meta("agent-1").is_some()
    });
    side_by_side(&shell, || serve_thread(&host, "thread-2"), cx);

    cx.simulate_keystrokes("secondary-alt-right");
    crate::test_frames::draw_settled(cx);
    store.update(cx, |store, cx| store.show_thread("agent-1".into(), cx));
    crate::test_frames::draw_settled(cx);
    serve_thread(&host, "agent-1");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("agent-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    cx.simulate_keystrokes("secondary-alt-left");
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("agent-1".into())]
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(0));

    let link = cx
        .debug_bounds("agent-parent-link")
        .expect("the way back to the lead");
    press_with_a_frame_between(link.center(), cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(1));
}

/// ⇧⌥⌘← exchanges the right pane's thread with the left one's, a
/// half-written message going along with it, and the keyboard stays in
/// the right pane, now on the left one's thread.
#[gpui::test]
fn swapping_panes_takes_the_thread_and_its_draft_along(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, host, cx) = mount_panes(cx);
    open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(1));
    cx.simulate_input("half a thought");
    draw(cx);

    cx.simulate_keystrokes("secondary-alt-shift-left");
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-2".into()), Some("thread-1".into())]
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(1), "the keyboard stays");
    assert_eq!(typed_reaches(&host, cx), ["thread-1"], "on thread-1");
    let left = shell.read_with(cx, |shell, cx| {
        shell.column.as_ref().expect("attached").all_panes(cx)[0].clone()
    });
    type_into(&left, cx);
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(
        turns_sent(&host),
        [("thread-2".to_owned(), "half a thought".to_owned())]
    );
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-1"));
        assert!(store.is_thread_open("thread-1"));
        assert!(store.is_thread_open("thread-2"));
    });
}

/// Moving the right pane's thread left trades it with the left pane's:
/// the moved thread shows in the left pane with the keyboard, the left
/// pane's thread in the right pane, and nothing closes.
#[gpui::test]
fn moving_a_thread_trades_places_with_its_neighbour(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(1));

    let mac = cx.update(|_, cx| cx.platform_info().is_mac());
    cx.simulate_keystrokes(if mac { "cmd-ctrl-left" } else { "alt-left" });
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-2".into()), Some("thread-1".into())]
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-2"));
        assert!(store.is_thread_open("thread-1"));
        assert!(store.is_thread_open("thread-2"));
    });
}

/// With the keyboard in a thread's terminal, the pane keys are the
/// terminal's: the layout's and the split's keys leave the panes alone.
#[gpui::test]
fn pane_keys_in_a_terminal_leave_the_panes_alone(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount_panes(cx);
    store_of(&shell, cx).update(cx, |store, cx| {
        store.toggle_terminal_panel_for("thread-1".to_owned(), cx)
    });
    draw(cx);
    let terminal = shell.read_with(cx, |shell, cx| {
        shell
            .column
            .as_ref()
            .expect("attached")
            .chat(cx)
            .read(cx)
            .terminal_drawer()
            .focus_handle(cx)
    });
    cx.update(|window, cx| window.focus(&terminal, cx));
    draw(cx);
    cx.update(|window, _| {
        assert!(
            window
                .context_stack()
                .iter()
                .any(|context| context.contains(crate::terminal_drawer::CONTEXT)),
            "the terminal has the keyboard"
        )
    });

    cx.simulate_keystrokes("secondary-alt-2 secondary-d");
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    cx.update(|window, _| assert!(terminal.is_focused(window)));
}

/// Two columns fit beside the sidebar of a 1200pt window. A split the
/// window has no room for is refused; stacking still fits.
#[gpui::test]
fn a_split_without_room_is_refused(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    resize(cx, 1200.);
    draw(cx);
    let _ = sent(&host);
    cx.simulate_keystrokes("secondary-d");
    draw(cx);
    answer_draft(&shell, &host, "draft-1", cx);
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("draft-1".into())]
    );
    cx.simulate_keystrokes("secondary-shift-w");
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);

    resize(cx, SIDEBAR_WIDTH + 2. * PANE_MIN_WIDTH - 10.);
    draw(cx);
    // A thread carried from the pane's middle, where it would land, to
    // its right edge, which has no room: the drop there does nothing.
    let pane = only_pane(&shell, cx);
    drag_row("sidebar-thread-thread-2", pane.center(), cx);
    assert!(cx.debug_bounds("pane-drop-indicator").is_some());
    let edge = gpui::point(pane.right() - px(20.), pane.center().y);
    cx.simulate_mouse_move(edge, Some(MouseButton::Left), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("pane-drop-indicator").is_none());
    cx.simulate_mouse_up(edge, MouseButton::Left, gpui::Modifiers::default());
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    let _ = sent(&host);

    cx.simulate_keystrokes("secondary-d");
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    assert!(
        !sent(&host)
            .iter()
            .any(|payload| matches!(payload, ClientPayload::Command(Command::StartScratchDraft)))
    );

    cx.simulate_keystrokes("secondary-shift-d");
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
}

/// A thread archived while another pane has the keyboard takes its pane
/// with it; the focused thread is left alone.
#[gpui::test]
fn archiving_a_background_panes_thread_closes_the_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(1));

    send_events(&host, vec![panes_index(&["thread-1"])]);
    await_restore_update(&shell, cx, |store, _| !store.is_thread_open("thread-1"));
    draw(cx);
    // It glides shut first.
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-2"));
    });
}

/// An archived agent leaves its pane however the panes stand: beside
/// others the pane goes; alone it stays and shows the lead.
#[gpui::test]
fn an_archived_agent_gives_way_to_its_lead(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, host, cx) = mount_lead_with_agent(cx);
    let store = store_of(&shell, cx);
    let archive_agent = |archived: bool| {
        let (topic, ServerEvent::IndexSnapshot(mut index)) = lead_with_agent_index("thread-1")
        else {
            unreachable!("the index is a snapshot");
        };
        index
            .sessions
            .iter_mut()
            .find(|meta| meta.id == "agent-1")
            .expect("the agent")
            .archived_at = archived.then_some(3);
        (topic, ServerEvent::IndexSnapshot(index))
    };
    cx.update(|window, cx| {
        let agent = store.read(cx).thread_key("agent-1");
        window.dispatch_action(Box::new(OpenInNewPane(agent)), cx)
    });
    draw(cx);
    serve_thread(&host, "agent-1");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("agent-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("agent-1".into())]
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(1));

    send_events(&host, vec![archive_agent(true)]);
    await_restore_update(&shell, cx, |store, _| !store.is_thread_open("agent-1"));
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);

    send_events(&host, vec![archive_agent(false)]);
    await_restore_update(&shell, cx, |store, _| {
        store
            .sidebar_sessions()
            .iter()
            .any(|meta| meta.id == "agent-1")
    });
    store.update(cx, |store, cx| store.show_thread("agent-1".into(), cx));
    serve_thread(&host, "agent-1");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("agent-1"), cx)
    });
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("agent-1".into())]);

    send_events(&host, vec![archive_agent(true)]);
    await_restore_update(&shell, cx, |store, _| !store.is_thread_open("agent-1"));
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
}

/// Closing a pane closes its thread, which stays listed, and the store
/// moves on to the thread of the pane the keyboard lands in.
#[gpui::test]
fn closing_a_pane_focuses_its_neighbours_thread(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.simulate_keystrokes("secondary-d");
    draw(cx);
    answer_draft(&shell, &host, "draft-1", cx);
    cx.simulate_keystrokes("secondary-alt-left");
    draw(cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));

    cx.simulate_keystrokes("secondary-shift-w");
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("draft-1".into())]);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(store.is_thread_open("draft-1"));
        assert!(!store.is_thread_open("thread-1"));
    });
    assert!(cx.debug_bounds("sidebar-thread-thread-1").is_some());

    cx.simulate_keystrokes("secondary-shift-w");
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("draft-1".into())],
        "the last pane stays"
    );
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

/// Any press inside a pane makes it current, whatever takes the press,
/// whether it reaches the pane or not, and the keyboard goes only where
/// that press sends it. The next thread lands there.
#[gpui::test]
fn a_press_anywhere_in_a_pane_makes_it_current(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let (topic, ServerEvent::IndexSnapshot(mut index)) = lead_with_agent_index("thread-1") else {
        unreachable!("an index")
    };
    let mut third = index.sessions[1].clone();
    third.id = "thread-3".into();
    third.title = "thread-3".into();
    third.updated_at = 0;
    index.sessions.push(third);
    send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
    await_restore_update(&shell, cx, |store, _| {
        store.thread_meta("thread-3").is_some()
    });
    open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
    let store = store_of(&shell, cx);
    let panes = pane_bounds(&shell, cx);
    let current = |left: bool, what: &str, cx: &mut VisualTestContext| {
        draw(cx);
        let thread = if left { "thread-1" } else { "thread-2" };
        let shown = shell.read_with(cx, |shell, cx| {
            shell.page_thread(cx).map(|thread| thread.session_id)
        });
        assert_eq!(shown.as_deref(), Some(thread), "{what}");
    };
    current(false, "the pane it opened in", cx);

    // A blank timeline sends the keyboard along.
    cx.simulate_click(panes[0].center(), gpui::Modifiers::default());
    current(true, "the left timeline", cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));

    // A header control stops the press, and the keyboard stays.
    let toggle = cx
        .debug_bounds("toggle-sidebar")
        .expect("the right header's");
    assert!(
        panes[1].contains(&toggle.center()),
        "{toggle:?} in {panes:?}"
    );
    press_only(toggle.center(), cx);
    current(false, "a control in the right header", cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(0), "the keyboard stays");
    // It leaves the composer and stays where it is pressed from.
    cx.update(|window, cx| window.blur(cx));
    current(false, "the keyboard leaving", cx);

    // The drawer, its pane's own.
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", RightTab::Diff, cx)
    });
    crate::test_frames::draw_settled(cx);
    current(false, "a drawer opening elsewhere", cx);
    let drawer = cx.debug_bounds("right-panel").expect("the drawer");
    press_only(drawer.center(), cx);
    current(true, "its drawer", cx);
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", RightTab::Diff, cx)
    });
    crate::test_frames::draw_settled(cx);

    // The card of an agent beside its lead.
    let chat = shell.read_with(cx, |shell, cx| {
        shell.column.as_ref().unwrap().panes(cx)[0].read(cx).chat()
    });
    cx.update(|window, cx| {
        crate::agents_panel::open_agent(
            &store,
            "agent-1".into(),
            Some(chat.clone()),
            None,
            window,
            cx,
        );
    });
    draw(cx);
    serve_thread(&host, "agent-1");
    await_restore_update(&shell, cx, |store, _| store.timeline_settled_for("agent-1"));
    crate::test_frames::draw_settled(cx);
    press_only(panes[1].center(), cx);
    current(false, "the right timeline", cx);
    let card = cx
        .debug_bounds("chat-side-title")
        .expect("the agent's card");
    press_only(card.center(), cx);
    current(true, "the agent's card", cx);

    press_only(panes[1].center(), cx);
    store.update(cx, |store, cx| store.show_thread("thread-3".into(), cx));
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx)[1],
        Some("thread-3".into()),
        "the next thread lands in the current pane"
    );
    serve_thread(&host, "thread-3");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-3"), cx)
    });
    crate::test_frames::draw_settled(cx);

    // A zoomed pane is current: here the one holding the keyboard, zoomed
    // from it with the other pane current.
    cx.simulate_click(panes[0].center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    let toggle = cx
        .debug_bounds("toggle-sidebar")
        .expect("the right header's");
    press_only(toggle.center(), cx);
    let shown = shell.read_with(cx, |shell, cx| {
        shell.page_thread(cx).map(|thread| thread.session_id)
    });
    assert_eq!(shown.as_deref(), Some("thread-3"));
    cx.simulate_keystrokes("secondary-shift-enter");
    crate::test_frames::draw_settled(cx);
    current(true, "the pane zoomed", cx);
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

/// One conversation on screen is simply current: its row stays lit
/// wherever the keyboard goes, an agent beside it lights the same row,
/// and a route without the conversation lights none.
#[gpui::test]
fn a_lone_panes_row_stays_lit_wherever_the_keyboard_goes(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    let row = "sidebar-thread-thread-1";
    let lit = cx.update(|_, cx| cx.theme().list_active.a);
    let settled = |cx: &mut VisualTestContext| {
        crate::test_frames::draw_settled(cx);
        row_fill(row, cx)
    };
    assert_eq!(settled(cx), lit, "the composer");

    cx.update(|window, cx| window.blur(cx));
    assert_eq!(settled(cx), lit, "nothing holds the keyboard");

    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", RightTab::Diff, cx)
    });
    draw(cx);
    assert_eq!(settled(cx), lit, "a docked panel leaves the keyboard alone");
    let panel = cx.debug_bounds("right-panel").expect("the drawer");
    cx.simulate_click(panel.center(), gpui::Modifiers::default());
    assert_eq!(settled(cx), lit, "its drawer");
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", RightTab::Diff, cx)
    });
    crate::test_frames::draw_settled(cx);

    store.update(cx, |store, cx| {
        store.toggle_terminal_panel_for("thread-1".to_owned(), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(settled(cx), lit, "its terminal");
    store.update(cx, |store, cx| {
        store.toggle_terminal_panel_for("thread-1".to_owned(), cx)
    });
    crate::test_frames::draw_settled(cx);

    let chat = shell.read_with(cx, |shell, cx| shell.column.as_ref().unwrap().chat(cx));
    cx.update(|window, cx| {
        crate::agents_panel::open_agent(
            &store,
            "agent-1".into(),
            Some(chat.clone()),
            None,
            window,
            cx,
        );
    });
    draw(cx);
    serve_thread(&host, "agent-1");
    await_restore_update(&shell, cx, |store, _| store.timeline_settled_for("agent-1"));
    assert_eq!(settled(cx), lit, "the agent beside it lights its lead");

    let state = shell.read_with(cx, |shell, _| shell.window_state());
    state.update(cx, |state, cx| state.open_palette(cx));
    draw(cx);
    assert_eq!(settled(cx), lit, "under the palette");
    state.update(cx, |state, cx| state.close_palette(cx));
    crate::test_frames::draw_settled(cx);

    state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
    draw(cx);
    assert_eq!(settled(cx), 0., "no conversation is on screen");
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

/// Among panes one is current, and its row is the one selected. A press
/// on the other pane's text makes that pane current and leaves its
/// composer without the keyboard. The keyboard leaving and the palette
/// move nothing; a zoomed pane is current; a route without the
/// conversation selects no row.
#[gpui::test]
fn a_press_in_a_pane_makes_its_row_current(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, host, cx) = mount_panes(cx);
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
    draw(cx);
    serve_status(&host, "thread-2", false);
    serve_answer(&host, "thread-2");
    await_restore_update(&shell, cx, |store, _| {
        store.timeline_settled_for("thread-2")
    });
    crate::test_frames::draw_settled(cx);
    let full = cx.update(|_, cx| cx.theme().list_active.a);
    let selected = |cx: &mut VisualTestContext| {
        crate::test_frames::draw_settled(cx);
        [
            row_fill("sidebar-thread-thread-1", cx) == full,
            row_fill("sidebar-thread-thread-2", cx) == full,
        ]
    };
    let panes = pane_bounds(&shell, cx);
    assert_eq!(selected(cx), [false, true]);
    cx.simulate_click(panes[0].center(), gpui::Modifiers::default());
    assert_eq!(selected(cx), [true, false]);
    assert_eq!(typed_reaches(&host, cx), ["thread-1"]);

    // A press on the right timeline's text: where a selection starts.
    let text = cx.debug_bounds("markdown-block-0").expect("the answer");
    let answer = gpui::point(text.left() + px(30.), text.center().y);
    cx.simulate_mouse_down(answer, MouseButton::Left, gpui::Modifiers::default());
    cx.simulate_mouse_up(answer, MouseButton::Left, gpui::Modifiers::default());
    assert_eq!(selected(cx), [false, true]);
    assert_eq!(typed_reaches(&host, cx), Vec::<String>::new());
    cx.update(|window, cx| window.blur(cx));
    assert_eq!(selected(cx), [false, true]);

    let state = shell.read_with(cx, |shell, _| shell.window_state());
    state.update(cx, |state, cx| state.open_palette(cx));
    assert_eq!(selected(cx), [false, true], "under the palette");
    state.update(cx, |state, cx| state.close_palette(cx));

    cx.update(|window, cx| window.dispatch_action(Box::new(crate::panes::ZoomPane), cx));
    assert_eq!(selected(cx), [false, true], "zoomed");
    cx.update(|window, cx| window.dispatch_action(Box::new(crate::panes::ZoomPane), cx));

    state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
    assert_eq!(selected(cx), [false, false], "no conversation is on screen");
}

/// A shown thread's row makes it current with the keyboard in its
/// composer and the layout as it is; another thread's replaces the
/// current pane's, whichever pane holds the keyboard.
#[gpui::test]
fn a_rows_click_goes_to_its_shown_thread_or_replaces_the_current_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, host, cx) = mount_panes(cx);
    let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
        unreachable!("an index")
    };
    let mut third = index.sessions[1].clone();
    third.id = "thread-3".into();
    third.title = "thread-3".into();
    third.updated_at = 0;
    index.sessions.push(third);
    send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
    await_restore_update(&shell, cx, |store, _| {
        store.thread_meta("thread-3").is_some()
    });
    open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
    let click = |selector: &'static str, cx: &mut VisualTestContext| {
        let row = cx.debug_bounds(selector).expect("the thread's row");
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        draw(cx);
    };
    click("sidebar-thread-thread-1", cx);
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    assert_eq!(typed_reaches(&host, cx), ["thread-1"]);

    // The right pane pressed where the keyboard stays out of it.
    let toggle = cx
        .debug_bounds("toggle-sidebar")
        .expect("the right header's");
    press_only(toggle.center(), cx);
    assert_eq!(typed_reaches(&host, cx), ["thread-1"]);

    click("sidebar-thread-thread-3", cx);
    serve_thread(&host, "thread-3");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-3"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-3".into())]
    );
}

/// A thread restored at launch takes the keyboard once it has loaded,
/// with no further input, though it was asked for before the window was
/// active and before its composer was drawn.
#[gpui::test]
fn a_restored_thread_takes_the_keyboard_once_loaded(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, _, cx) =
        mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 1024., "plan");
    assert!(!cx.update(|window, _| window.is_window_active()));
    assert!(cx.debug_bounds("baseline-loading").is_some());

    restore_index(&shell, &host, true, cx);
    restore_status(&shell, &host, cx);
    restore_settings_and_events(&shell, &host, cx);
    draw(cx);
    assert!(!cx.update(|window, _| window.is_window_active()));
    assert_eq!(typed_reaches(&host, cx), ["thread-a"]);
}

/// Choosing a thread still loading gives its composer the keyboard the
/// moment it is drawn; meanwhile the keyboard is in the pane, which it
/// leaves to nothing else.
#[gpui::test]
fn a_chosen_threads_composer_takes_the_keyboard_as_it_is_drawn(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|window, cx| window.blur(cx));
    draw(cx);
    let row = cx
        .debug_bounds("sidebar-thread-thread-2")
        .expect("the thread's row");
    cx.simulate_click(row.center(), gpui::Modifiers::default());
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));

    serve_thread(&host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    draw(cx);
    assert_eq!(typed_reaches(&host, cx), ["thread-2"]);
}

/// A thread the workspace goes to by itself finds the keyboard in a lone
/// pane with nothing holding it, and leaves it where something does.
#[gpui::test]
fn an_unasked_thread_takes_only_a_keyboard_nothing_holds(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    serve_thread(&host, "thread-2");
    cx.update(|window, cx| window.blur(cx));
    store.update(cx, |store, cx| store.put_back_thread("thread-2".into(), cx));
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    draw(cx);
    assert_eq!(typed_reaches(&host, cx), ["thread-2"], "nothing held it");

    // The palette's input holds it.
    let state = shell.read_with(cx, |shell, _| shell.window_state());
    state.update(cx, |state, cx| state.open_palette(cx));
    crate::test_frames::draw_settled(cx);
    // Its query, inside it, holds the keyboard, not the palette itself.
    let palette = shell.read_with(cx, |shell, cx| shell.palette.focus_handle(cx));
    let query_held = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| palette.contains_focused(window, cx) && !palette.is_focused(window))
    };
    assert!(query_held(cx));
    serve_thread(&host, "thread-1");
    store.update(cx, |store, cx| store.put_back_thread("thread-1".into(), cx));
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    assert!(query_held(cx), "it keeps the keyboard");
    state.update(cx, |state, cx| state.close_palette(cx));
    crate::test_frames::draw_settled(cx);

    // Beside another pane it stays nowhere.
    open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
    cx.update(|window, cx| window.blur(cx));
    store.update(cx, |store, cx| store.put_back_thread("thread-2".into(), cx));
    crate::test_frames::draw_settled(cx);
    assert_eq!(keyboard_pane(&shell, cx), None);
}

/// What holds the keyboard over a conversation gives it to a composer as
/// it closes: at once, while it still leaves, not when it has gone. A
/// dialog with nothing to go back to does the same.
#[gpui::test]
fn closing_the_palette_or_a_dialog_returns_the_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let state = shell.read_with(cx, |shell, _| shell.window_state());
    state.update(cx, |state, cx| state.open_palette(cx));
    crate::test_frames::draw_settled(cx);
    state.update(cx, |state, cx| state.close_palette(cx));
    draw(cx);
    assert_eq!(typed_reaches(&host, cx), ["thread-1"], "the palette left");

    cx.update(|window, cx| {
        window.blur(cx);
        window.open_dialog(cx, |dialog, _, _| dialog);
    });
    draw(cx);
    cx.update(|window, cx| window.close_dialog(cx));
    draw(cx);
    assert_eq!(typed_reaches(&host, cx), ["thread-1"], "the dialog closed");
}

/// Among several panes each pane's header ends with its close button,
/// which goes as the column is left with one; pressing it closes its
/// own pane.
#[gpui::test]
fn a_panes_close_button_closes_its_own_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    assert!(cx.debug_bounds("pane-close").is_none(), "a lone pane");

    // Stacked; the new pane waits for its draft.
    cx.simulate_keystrokes("secondary-shift-d");
    crate::test_frames::draw_settled(cx);
    assert!(
        cx.debug_bounds("pane-close").is_some(),
        "the pane's close button"
    );

    cx.simulate_keystrokes("secondary-shift-w");
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    assert!(
        cx.debug_bounds("pane-close").is_none(),
        "gone with the second pane"
    );

    // The draft the closed pane waited for is no longer wanted.
    let _ = sent(&host);
    cx.simulate_keystrokes("secondary-shift-d");
    draw(cx);
    answer_draft(&shell, &host, "draft-1", cx);
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("draft-1".into())]
    );
    cx.simulate_keystrokes("secondary-alt-up");
    draw(cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    let close = cx.debug_bounds("pane-close").expect("a close button");
    cx.simulate_click(close.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-1"));
        assert!(!store.is_thread_open("draft-1"));
    });
}

/// A thread goes to the pane showing it, or else to the pane of the
/// control that asked, or else to the pane last in use, whichever pane
/// the keyboard has left it in, and takes the keyboard there.
#[gpui::test]
fn a_shown_thread_lands_by_the_placement_order(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);

    store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())],
        "a thread a pane shows stays there"
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(0));

    store.update(cx, |store, cx| {
        store.show_thread_replacing("thread-3".into(), Some("thread-2".into()), cx)
    });
    serve_thread(&host, "thread-3");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-3"), cx)
    });
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-3".into())],
        "the asking control's pane, with the keyboard in the other"
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(1));
    store.read_with(cx, |store, _| {
        assert!(!store.is_thread_open("thread-2"), "the thread it replaced");
    });

    store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    draw(cx);
    cx.update(|window, cx| window.blur(cx));
    store.update(cx, |store, cx| store.show_thread("thread-2".into(), cx));
    serve_thread(&host, "thread-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-2"), cx)
    });
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-2".into()), Some("thread-3".into())],
        "the current pane, with the keyboard nowhere"
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
}

/// An agent shown beside its lead is chosen where it is: its card takes
/// the keyboard and no pane takes the agent in the lead's place.
#[gpui::test]
fn choosing_an_agent_shown_beside_its_lead_selects_its_card(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    let chat = shell.read_with(cx, |shell, cx| shell.column.as_ref().unwrap().chat(cx));
    cx.update(|window, cx| {
        crate::agents_panel::open_agent(
            &store,
            "agent-1".into(),
            Some(chat.clone()),
            None,
            window,
            cx,
        );
    });
    crate::test_frames::draw_settled(cx);
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    draw(cx);

    store.update(cx, |store, cx| store.show_thread("agent-1".into(), cx));
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    assert_eq!(
        typed_reaches(&host, cx),
        Vec::<String>::new(),
        "the lead's composer let it go"
    );
}

/// Choosing a project in the chooser closes it and shows the draft the
/// host names, the same one when asked again; a phone goes to it. The
/// chooser and the draft raise the keyboard only off a phone, where it is
/// typed into at once; there a tap does, and keeps it across a rotation.
/// One project needs no choosing.
#[gpui::test]
fn project_choice_reuses_drafts_and_focuses_only_on_desktop(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    for (mobile, compact, projects) in [
        (false, false, 2),
        (true, false, 2),
        (true, true, 2),
        (true, true, 1),
    ] {
        new_thread_from_projects(cx, mobile, compact, projects);
    }
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

/// The draft an empty workspace falls back on comes up in its pane and
/// leaves a phone on its threads list: nobody asked to go there.
#[gpui::test]
fn a_fallback_draft_leaves_a_phone_on_its_threads(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    as_mobile(cx);
    resize(cx, 393.);
    let project = tcode_core::project::Project {
        id: "project".into(),
        name: "Project".into(),
        root: PANES_CWD.into(),
        quick_chat: false,
        icon_path: None,
        permission_defaults: Default::default(),
        created_at: 0,
    };
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
                    projects: vec![project],
                }),
            ),
        ],
    );
    await_restore_update(&shell, cx, |store, _| store.index_hydrated());
    draw(cx);
    let request = std::iter::from_fn(|| host.outgoing.try_recv().ok())
        .map(|line| decode_client_line(&line).expect("client line"))
        .find(|message| {
            matches!(
                message.payload,
                ClientPayload::Command(Command::StartDraft { .. })
            )
        })
        .expect("the workspace asks for a draft");
    host.incoming
        .try_send(
            encode_line(&HostMessage::Ack {
                id: request.id,
                result: Ok(tcode_protocol::CommandResponse::SessionId(Some(
                    "draft-1".into(),
                ))),
            })
            .unwrap(),
        )
        .unwrap();
    await_restore_update(&shell, cx, |store, _| store.is_thread_open("draft-1"));
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("draft-1".into())]);
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.destination(cx), Destination::Threads);
    });
}

/// A column showing nothing opens the remembered project's new thread
/// page instead of a dead empty page, though another project is listed
/// first; a host with no project at all is asked for nothing and the
/// column keeps its add-project state.
#[gpui::test]
fn an_empty_column_opens_the_remembered_projects_draft(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let project = |id: &str| crate::store::tests::project_at(id, std::path::Path::new(PANES_CWD));
    for (projects, asked) in [
        (
            vec![project("first"), project("remembered")],
            vec!["remembered"],
        ),
        (Vec::new(), Vec::new()),
    ] {
        let (shell, host, cx) = mount(cx);
        send_events(
            &host,
            vec![
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                        last_project_id: Some("remembered".into()),
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
        await_restore_update(&shell, cx, |store, _| store.index_hydrated());
        draw(cx);
        let drafted: Vec<String> = std::iter::from_fn(|| host.outgoing.try_recv().ok())
            .filter_map(
                |line| match decode_client_line(&line).expect("client line").payload {
                    ClientPayload::Command(Command::StartDraft { project_id, .. }) => {
                        Some(project_id)
                    }
                    _ => None,
                },
            )
            .collect();
        assert_eq!(drafted, asked);
        assert_eq!(pane_threads(&shell, cx), [None]);
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

/// A profile from before theme, language and the sidebar were this
/// device's: each takes the first host's value once, and the host
/// changing its own later moves none of them, its language included.
#[gpui::test]
fn an_older_profile_takes_its_device_preferences_from_the_host_once(cx: &mut TestAppContext) {
    use crate::store::tests::scratch_root;
    use tcode_core::settings::{Settings, SettingsPatch, SidebarLayout, ThemeMode};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let root = scratch_root("device-preferences-migration");
    let disk = tcode_services::store::SessionStore::open_at(root.clone()).expect("open test store");
    tcode_services::settings::SettingsStore::new(root.clone())
        .save(&Settings {
            theme_mode: ThemeMode::Dark,
            language: Some(crate::LANGUAGE_JAPANESE.into()),
            sidebar_collapsed: true,
            sidebar_layout: SidebarLayout::Grouped,
            ..Settings::default()
        })
        .expect("save host settings");
    let device = root.join("device");
    std::fs::create_dir_all(&device).unwrap();
    std::fs::write(
        device.join("mobile.json"),
        br#"{"appearance": null, "language": null, "device_name": "Old phone"}"#,
    )
    .unwrap();
    let saved = || tcode_traverse::NativeClientHost::new(device.clone(), "phone");
    let (shell, store, host, mut driver, cx) = mount_on_host(cx, disk, Some(Rc::new(saved())));
    driver.wait_until(cx, "the host's settings", |cx| {
        store.read_with(cx, |store, _| store.settings_hydrated())
    });
    draw(cx);
    let device_preferences = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            (
                DevicePreferences::theme(cx),
                DevicePreferences::language(cx),
                DevicePreferences::sidebar_collapsed(cx),
                DevicePreferences::sidebar_layout(cx),
            )
        })
    };
    let adopted = (
        ThemeMode::Dark,
        Some(crate::LANGUAGE_JAPANESE.to_owned()),
        true,
        SidebarLayout::Grouped,
    );
    assert_eq!(device_preferences(cx), adopted);
    let persisted = saved().load_preferences().unwrap();
    assert_eq!(
        (
            persisted.appearance,
            persisted.language.as_deref(),
            persisted.sidebar_collapsed,
            persisted.sidebar_layout,
            persisted.device_name.as_deref(),
        ),
        (
            Some(ThemeMode::Dark),
            Some(crate::LANGUAGE_JAPANESE),
            Some(true),
            Some(SidebarLayout::Grouped),
            Some("Old phone"),
        )
    );
    assert!(cx.update(|_, cx| cx.theme().mode.is_dark()));
    assert_eq!(&*rust_i18n::locale(), crate::LANGUAGE_JAPANESE);

    let link = driver.link.clone();
    for command in [
        Command::PatchSettings {
            patch: SettingsPatch::ThemeMode(ThemeMode::Light),
        },
        Command::PatchSettings {
            patch: SettingsPatch::Language(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE.into())),
        },
        Command::PatchSettings {
            patch: SettingsPatch::SidebarLayout(SidebarLayout::Flat),
        },
        Command::SetSidebarCollapsed { collapsed: false },
    ] {
        driver
            .await_with_events(cx, link.command(command))
            .expect("the host takes the old patch");
    }
    driver.wait_until(cx, "the host's new settings", |cx| {
        store.read_with(cx, |store, _| {
            let settings = store.settings();
            settings.theme_mode == ThemeMode::Light
                && settings.language.as_deref() == Some(crate::LANGUAGE_SIMPLIFIED_CHINESE)
                && settings.sidebar_layout == SidebarLayout::Flat
                && !settings.sidebar_collapsed
        })
    });
    draw(cx);
    assert_eq!(device_preferences(cx), adopted);
    assert_eq!(saved().load_preferences().unwrap(), persisted);
    assert!(cx.update(|_, cx| cx.theme().mode.is_dark()));
    assert_eq!(&*rust_i18n::locale(), crate::LANGUAGE_JAPANESE);
    driver.shutdown(cx, &host);
    drop(shell);
    let _ = std::fs::remove_dir_all(root);
}

/// This device's preferences and saved machines in files this build
/// cannot read: the window opens on the host anyway, says which files and
/// why, and writes over neither, though the host's preferences are
/// adopted and the theme changes.
#[gpui::test]
fn unreadable_device_files_are_reported_and_left_as_they_are(cx: &mut TestAppContext) {
    use crate::store::tests::scratch_root;
    use tcode_core::settings::ThemeMode;
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let root = scratch_root("device-files-unreadable");
    let disk = tcode_services::store::SessionStore::open_at(root.clone()).expect("open test store");
    let device = root.join("device");
    std::fs::create_dir_all(&device).unwrap();
    let preferences = br#"{"appearance": "dark","#;
    let machines = br#"[{"host_id": 7}]"#;
    std::fs::write(device.join("mobile.json"), preferences).unwrap();
    std::fs::write(device.join("hosts.json"), machines).unwrap();
    let client = tcode_traverse::NativeClientHost::new(device.clone(), "phone");
    use tcode_client::host::ClientHost as _;
    // The window says each failure in the language it starts in, before
    // it adopts the host's.
    let said = [
        ("mobile.json", client.load_preferences().unwrap_err()),
        ("hosts.json", client.load_hosts().unwrap_err()),
    ]
    .map(|(file, failure)| {
        let failure = failure.to_string();
        assert!(failure.contains(file), "{file} is named: {failure}");
        crate::tr!("errors.file_unreadable", error = failure).into_owned()
    });
    let (shell, store, host, mut driver, cx) = mount_on_host(cx, disk, Some(Rc::new(client)));
    driver.wait_until(cx, "the host's settings", |cx| {
        store.read_with(cx, |store, _| store.settings_hydrated())
    });
    draw(cx);
    for said in said {
        assert!(
            cx.debug_bounds(format!("toast-says-{said}").leak())
                .is_some(),
            "the window says {said}"
        );
    }
    cx.update(|window, cx| DevicePreferences::set_theme(ThemeMode::Light, window, cx));
    draw(cx);
    assert!(cx.update(|_, cx| !cx.theme().mode.is_dark()));
    assert_eq!(
        std::fs::read(device.join("mobile.json")).unwrap(),
        preferences
    );
    assert_eq!(std::fs::read(device.join("hosts.json")).unwrap(), machines);
    driver.shutdown(cx, &host);
    drop(shell);
    let _ = std::fs::remove_dir_all(root);
}

/// Deleting the project of the draft on screen takes the draft away; the
/// pane takes the draft of a project still listed rather than emptying.
#[gpui::test]
fn deleting_the_shown_drafts_project_opens_another_projects_draft(cx: &mut TestAppContext) {
    use crate::store::tests::{project_at, scratch_root, thread};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let root = scratch_root("remove-shown-draft-project");
    let disk = tcode_services::store::SessionStore::open_at(root.clone()).expect("open test store");
    for project in ["doomed", "kept"] {
        disk.upsert_project(&project_at(project, &root))
            .expect("persist project");
    }
    disk.upsert_meta(&thread(&root, "kept-thread", "kept", None))
        .expect("persist session");
    let (shell, store, host, mut driver, cx) = mount_on_host(cx, disk, None);
    let shows_draft_of = |project: &'static str| {
        let store = store.clone();
        move |cx: &TestAppContext| {
            store.read_with(cx, |store, cx| {
                store.sole_status(cx).is_some_and(|status| {
                    status.draft && status.project_id.as_deref() == Some(project)
                })
            })
        }
    };

    store.update(cx, |store, cx| {
        store.start_draft("doomed".into(), root.clone(), cx)
    });
    driver.wait_until(cx, "draft for the doomed project", shows_draft_of("doomed"));
    store.update(cx, |store, _| store.delete_project("doomed".into()));
    driver.wait_until(cx, "kept project's draft", shows_draft_of("kept"));
    let draft = store.read_with(cx, |store, _| store.sole_thread());
    assert_eq!(pane_threads(&shell, cx), [draft]);

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}

/// Archiving a parent archives its children in one batch. A pane showing
/// one of those children has no visible parent to return to, so it takes
/// the standing draft of the last interacted project: the same draft
/// session id, so its composer state survives.
#[gpui::test]
fn batch_archive_without_a_visible_parent_reopens_the_standing_draft(cx: &mut TestAppContext) {
    use crate::store::tests::{archived, project_at, scratch_root, thread};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let root = scratch_root("archive-to-draft");
    let disk = tcode_services::store::SessionStore::open_at(root.clone()).expect("open test store");
    // "other" is listed first, so a draft for it proves nothing about the
    // remembered project; "p" is the one the user last worked in.
    disk.upsert_project(&project_at("other", &root.join("other")))
        .expect("persist project");
    disk.upsert_project(&project_at("p", &root))
        .expect("persist project");
    for meta in [
        thread(&root, "parent", "p", None),
        thread(&root, "child", "p", Some("parent")),
    ] {
        disk.upsert_meta(&meta).expect("persist session");
    }
    let (shell, store, host, mut driver, cx) = mount_on_host(cx, disk, None);

    // Let the launch draft settle on the first project before the user
    // goes into "p" themselves.
    driver.wait_until(cx, "launch draft", |cx| {
        store.read_with(cx, |store, _| store.sole_thread().is_some())
    });
    store.update(cx, |store, cx| {
        store.start_draft("p".into(), root.clone(), cx)
    });
    driver.wait_until(cx, "draft for the last project", |cx| {
        store.read_with(cx, |store, cx| {
            store
                .sole_status(cx)
                .is_some_and(|status| status.draft && status.project_id.as_deref() == Some("p"))
        })
    });
    let draft_id = store
        .read_with(cx, |store, _| store.sole_thread())
        .expect("draft shown");
    store.update(cx, |store, cx| {
        store.open_tab_for(&draft_id, RightTab::Diff, cx)
    });

    store.update(cx, |store, cx| store.show_thread("child".into(), cx));
    driver.wait_until(cx, "child shown", |cx| {
        store.read_with(cx, |store, cx| {
            store
                .sole_status(cx)
                .is_some_and(|status| status.session_id == "child")
        })
    });

    store.update(cx, |store, _| store.archive_session("parent".into()));
    driver.wait_until(cx, "the standing draft reopened", |cx| {
        store.read_with(cx, |store, _| {
            store.sole_thread().as_deref() == Some(draft_id.as_str())
        })
    });
    assert_eq!(pane_threads(&shell, cx), [Some(draft_id.clone())]);
    store.read_with(cx, |store, _| {
        assert!(
            store.panel_state_for(Some(&draft_id)).right_panel_open,
            "the reopened draft lost the state the user left in it"
        );
    });
    assert!(archived(cx, &store, "parent"));
    assert!(archived(cx, &store, "child"));

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}

#[gpui::test]
fn header_controls_activate_after_pointer_jitter_without_dragging(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    for (topic, event) in [
        (
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions: Vec::new(),
                projects: Vec::new(),
            }),
        ),
        (
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
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
    await_restore_update(&shell, cx, |store, cx| !store.chat_loading_for(None, cx));
    let state = shell.read_with(cx, |shell, _| shell.window_state());
    for (width, selector) in [(1024., "toggle-sidebar"), (393., "compact-search")] {
        if width < 900. {
            as_mobile(cx);
        }
        resize(cx, width);
        crate::test_frames::draw_settled(cx);
        let start = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing {selector}"))
            .center();
        let end = start + gpui::point(px(1.), px(1.));
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        // TestWindow rejects native window moves, so a leaked press fails here.
        cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
        state.read_with(cx, |state, cx| {
            if width >= 900. {
                assert!(crate::device::DevicePreferences::sidebar_collapsed(cx));
            } else {
                assert!(state.palette_open);
            }
        });
    }
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

#[gpui::test]
fn connecting_from_the_hosts_panel_releases_its_entity_before_attachment(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let studio = saved_machine("studio", false);
    let (shell, _hosts, _client, cx) = mount_workspace(
        cx,
        vec![studio.clone()],
        tcode_client::host::ClientPreferences {
            workspace: Some(Default::default()),
            ..Default::default()
        },
        393.,
    );
    shell
        .read_with(cx, |shell, _| shell.window_state())
        .update(cx, |state, cx| {
            state.go(Destination::Thread, cx);
            state.go(Destination::Panel, cx);
        });
    let hosts = shell.read_with(cx, |shell, _| shell.hosts.clone());
    cx.update(|window, cx| {
        // Connect runs inside RemotePanel's click listener. Opening a
        // host updates that same panel with the new store.
        hosts.update(cx, |_, cx| {
            request_host(
                HostRequest::Open(AttachmentTarget::Remote(Box::new(studio))),
                window,
                cx,
            );
        });
    });
    draw(cx);
    shell.read_with(cx, |shell, cx| {
        assert!(shell.store(cx).is_some());
        assert_eq!(
            shell.window_state.read(cx).history(),
            [Destination::Threads]
        );
    });
    assert!(!cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
}

/// A local host that did not start at launch is on Machines from the
/// first frame, whatever the window attached to. Retry asks bootstrap to
/// start it again; a failed start keeps the machine the window is on, and
/// one that comes up joins it.
#[gpui::test]
fn a_local_host_that_does_not_start_is_shown_and_retried(cx: &mut TestAppContext) {
    struct Desk {
        saved: tcode_client::pairing::PairedHost,
        transports: RefCell<Vec<Transport>>,
    }
    impl ClientHost for Desk {
        fn device_name(&self) -> String {
            "desk".into()
        }
        fn device_id(&self) -> Result<String, tcode_client::host::FileFailure> {
            Ok("desk".into())
        }
        fn device_platform(&self) -> Option<String> {
            None
        }
        fn load_hosts(
            &self,
        ) -> Result<Vec<tcode_client::pairing::PairedHost>, tcode_client::host::FileFailure>
        {
            Ok(vec![self.saved.clone()])
        }
        fn save_hosts(
            &self,
            _: &[tcode_client::pairing::PairedHost],
        ) -> Result<(), tcode_client::host::FileFailure> {
            Ok(())
        }
        fn last_host_id(&self) -> Option<String> {
            None
        }
        fn set_last_host_id(&self, _: Option<&str>) {}
        fn pair(
            &self,
            _: tcode_client::pairing::PairInvite,
            _: Option<std::net::IpAddr>,
        ) -> tcode_client::host::HostFuture<
            '_,
            Result<tcode_client::pairing::PairedHost, tcode_client::pairing::PairError>,
        > {
            unreachable!("nothing is paired here")
        }
        fn connect(&self, _: &tcode_client::pairing::PairedHost) -> Transport {
            self.transports.borrow_mut().pop().expect("one connection")
        }
    }
    fn transport() -> (Transport, async_channel::Sender<String>) {
        let (to_host, _) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let (_, state) = async_channel::unbounded();
        let transport = Transport {
            originals: None,
            to_host: to_host.into(),
            from_host,
            state,
            current_host: None,
        };
        (transport, incoming)
    }

    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::test_client::init);
    let busy = "another Tcode host is already using the data directory /tmp/demo";
    let (local_transport, local_incoming) = transport();
    // Answered from the back: busy on the first Retry, up on the second.
    let answers = RefCell::new(vec![Ok(local_transport), Err(busy.to_owned())]);
    let attempts = Rc::new(Cell::new(0));
    let local: LocalTransport = {
        let attempts = attempts.clone();
        Rc::new(move |_| {
            attempts.set(attempts.get() + 1);
            answers.borrow_mut().pop().expect("an unexpected start")
        })
    };
    let saved = tcode_client::pairing::PairedHost {
        host_id: "studio".into(),
        name: "Studio".into(),
        traverse: Vec::new(),
        relay: None,
        addrs: vec!["127.0.0.1:47503".into()],
        last_connected_unix: None,
        group_id: None,
        space_id: None,
        space_name: None,
    };
    let (remote_transport, _remote_incoming) = transport();
    let desk: Rc<dyn ClientHost> = Rc::new(Desk {
        saved: saved.clone(),
        transports: RefCell::new(vec![remote_transport]),
    });
    cx.update(|cx| {
        cx.set_global(crate::remote::ClientAttachment::new(
            desk.clone(),
            true,
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
                    local: Some(local),
                    local_failure: Some(busy.to_owned()),
                    initial: Some(AttachmentTarget::Remote(Box::new(saved))),
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
    cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
    resize(cx, 1024.);
    let attached = |cx: &mut VisualTestContext| {
        shell.read_with(cx, |shell, cx| {
            shell
                .store(cx)
                .map(|store| store.read(cx).remote_host_id().map(str::to_owned))
        })
    };
    let open_machines = |cx: &mut VisualTestContext| {
        shell
            .read_with(cx, |shell, _| shell.window_state())
            .update(cx, |state, cx| state.go(Destination::Hosts, cx));
        draw(cx);
    };
    let retry = |cx: &mut VisualTestContext| {
        let button = cx.debug_bounds("hosts-local-retry").expect("Retry");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        draw(cx);
    };

    open_machines(cx);
    assert_eq!(attempts.get(), 0, "the launch's failure is not asked again");
    assert_eq!(attached(cx), Some(Some("studio".into())));
    assert!(cx.debug_bounds("hosts-local-failure").is_some());

    retry(cx);
    assert_eq!(attempts.get(), 1, "Retry starts the local host again");
    assert_eq!(
        attached(cx),
        Some(Some("studio".into())),
        "a failed start keeps the machine the window is on"
    );
    open_machines(cx);
    assert!(cx.debug_bounds("hosts-local-failure").is_some());

    retry(cx);
    assert_eq!(attempts.get(), 2);
    assert_eq!(
        attached(cx),
        Some(Some("studio".into())),
        "the window stays on the machine it is on"
    );
    let store = shell
        .read_with(cx, |shell, _| shell.local_store())
        .expect("the local host joins it");
    crate::store::tests::seed_full_scope(&store, &local_incoming, Vec::new(), cx);
    open_machines(cx);
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("hosts-local-failure").is_none());
}

/// The compact palette opens from the threads page and a thread as a
/// sheet on the window's bottom edge; the page beneath and what the sheet
/// holds keep clear of the status bar and the keyboard; Back dismisses
/// the keyboard before the sheet.
#[gpui::test]
fn the_compact_palette_opens_from_threads_and_thread_and_back_takes_the_keyboard_first(
    cx: &mut TestAppContext,
) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount(cx);
    as_mobile(cx);
    cx.simulate_resize(size(px(393.), px(852.)));
    crate::test_frames::draw_settled(cx);
    let state = shell.read_with(cx, |shell, _| shell.window_state());
    for selector in ["compact-search", "compact-open-palette"] {
        let button = cx.debug_bounds(selector).expect("compact palette entry");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        draw(cx);
        assert!(state.read_with(cx, |state, _| state.palette_open));
        let position = gpui::point(px(30.), px(26.));
        let history = state.read_with(cx, |state, _| state.history().to_vec());
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position,
            click_count: 1,
            ..Default::default()
        });
        draw(cx);
        cx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position,
            click_count: 1,
            ..Default::default()
        });
        assert!(!state.read_with(cx, |state, _| state.palette_open));
        assert_eq!(
            state.read_with(cx, |state, _| state.history().to_vec()),
            history
        );
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        draw(cx);
        assert!(state.read_with(cx, |state, _| state.palette_open));
        cx.update(|window, cx| {
            let palette = &shell.read(cx).palette;
            assert!(
                palette
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            );
        });
        // The status bar and the keyboard cover the window.
        crate::test_frames::occlude(
            cx,
            gpui::Edges {
                top: px(47.),
                bottom: px(400.),
                ..Default::default()
            },
        );
        crate::test_frames::draw_settled(cx);
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
            assert!(
                state.read(cx).palette_open,
                "Back dismisses the keyboard first"
            );
        });
        crate::test_frames::occlude(cx, gpui::Edges::default());
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
        });
        assert!(!state.read_with(cx, |state, _| state.palette_open));
        state.update(cx, |state, cx| state.go(Destination::Thread, cx));
        crate::test_frames::draw_settled(cx);
    }
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

#[gpui::test]
fn an_edge_swipe_closes_a_sheet_before_navigating_underneath(cx: &mut TestAppContext) {
    use gpui::TouchPhase::{Ended, Moved, Started};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount(cx);
    as_mobile(cx);
    resize(cx, 393.);
    cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
    cx.update(|window, cx| shell.update(cx, |shell, cx| while shell.back(window, cx) {}));
    draw(cx);
    assert!(!cx.cx.update(can_swipe_back));
    shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
    crate::test_frames::draw_settled(cx);
    let state = shell.read_with(cx, |shell, _| shell.window_state());
    let history = state.read_with(cx, |state, _| state.history().to_vec());
    state.update(cx, |state, cx| state.open_palette(cx));
    crate::test_frames::draw_settled(cx);
    assert!(cx.cx.update(can_swipe_back));
    touch_drag(
        cx,
        1,
        &[
            (Started, 10., 600., 0),
            (Moved, 95., 600., 200),
            (Moved, 180., 600., 200),
            (Ended, 180., 600., 10),
        ],
    );
    crate::test_frames::draw_settled(cx);
    assert!(!state.read_with(cx, |state, _| state.palette_open));
    assert_eq!(
        state.read_with(cx, |state, _| state.history().to_vec()),
        history
    );
    let depth = shell.read_with(cx, |shell, cx| shell.nav.read(cx).depth());
    assert!(cx.cx.update(handle_back));
    draw(cx);
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.nav.read(cx).depth(), depth - 1)
    });
}

#[gpui::test]
fn image_viewer_takes_back_before_the_page_underneath(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount(cx);
    as_mobile(cx);
    resize(cx, 393.);
    cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
    shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
    crate::test_frames::draw_settled(cx);
    assert!(cx.cx.update(can_swipe_back));
    let depth = shell.read_with(cx, |shell, cx| shell.nav.read(cx).depth());
    cx.update(|window, cx| {
        crate::image_viewer::open(
            gpui::ImageSource::Custom(std::sync::Arc::new(|_, _| None)),
            "image.png",
            window,
            cx,
        )
    });
    draw(cx);
    assert!(!cx.cx.update(can_swipe_back));
    assert!(cx.cx.update(handle_back));
    crate::test_frames::draw_settled(cx);
    assert!(!cx.update(|window, cx| crate::overlay::dialog_open(window, cx)));
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.nav.read(cx).depth(), depth)
    });
    assert!(cx.cx.update(can_swipe_back));
}

#[gpui::test]
fn compact_touch_drag_away_from_edge_preserves_page(cx: &mut TestAppContext) {
    use gpui::TouchPhase::{Ended, Moved, Started};

    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount(cx);
    as_mobile(cx);
    resize(cx, 393.);
    shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
    crate::test_frames::draw_settled(cx);
    let before = shell.read_with(cx, |shell, cx| {
        shell.window_state.read(cx).history().to_vec()
    });
    for (id, start_x, middle_x) in [(1, 60., 240.), (2, 60., 10.)] {
        // Catch an edge pan's fling with the next off-edge contact. GPUI
        // starts that contact directly as a pan, without a TouchDrag offer.
        touch_drag(
            cx,
            10 + id,
            &[
                (Started, 10., 300., 0),
                (Moved, 25., 300., 10),
                (Ended, 25., 300., 10),
            ],
        );
        touch_drag(
            cx,
            id,
            &[
                (Started, start_x, 300., 0),
                (Moved, middle_x, 300., 50),
                (Moved, 280., 300., 50),
                (Ended, 280., 300., 10),
            ],
        );
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.window_state.read(cx).history(), before);
            assert_eq!(shell.nav.read(cx).depth(), before.len());
        });
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

/// A desktop window, even a narrow one, gives the model picker's search
/// the keyboard as it opens; on a phone too the picker owns focus, so
/// Escape closes it.
#[gpui::test]
fn the_model_pickers_search_has_the_keyboard_on_a_desktop(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::test_client::init);
    let root = crate::store::tests::scratch_root("model-picker-focus");
    let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
    let project = tcode_core::project::Project::from_root(root.join("project"));
    let mut meta = tcode_core::project::SessionMeta::new(
        agent::ProviderKind::Codex,
        project.root.clone(),
        None,
    );
    meta.id = "first".into();
    meta.project_id = Some(project.id.clone());
    disk.upsert_project(&project).unwrap();
    disk.upsert_meta(&meta).unwrap();
    disk.save_models(
        agent::ProviderKind::Codex,
        &[agent::ModelSpec {
            id: "listed-model".into(),
            display_name: "Listed model".into(),
            is_default: true,
            options: Vec::new(),
        }],
    )
    .unwrap();
    let host = tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
        .unwrap();
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let (shell, _transport, cx) = mount(cx);
    cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| {
        *store = WorkspaceStore::new_attached(
            driver.link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        );
        store.show_thread("first".into(), cx);
    });
    driver.wait_until(cx, "selected thread baseline", |cx| {
        store.read_with(cx, |store, cx| {
            store.baseline_ready()
                && !store.threads_loading()
                && !store.chat_loading_for(Some("first"), cx)
        })
    });

    for (mobile, width) in [(false, 1200.), (false, 393.), (true, 393.)] {
        if mobile {
            cx.update(|_, cx| crate::window_seam::force_mobile_layout(cx));
        }
        resize(cx, width);
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        // Let the compact push animation settle so the picker is in place.
        crate::test_frames::draw_settled(cx);
        if !mobile && width < 900. {
            // At a phone's width the sidebar leaves the thread's row no
            // room for the picker; it is folded away to reach it.
            let toggle = cx.debug_bounds("toggle-sidebar").expect("sidebar toggle");
            cx.simulate_click(toggle.center(), gpui::Modifiers::default());
            crate::test_frames::draw_settled(cx);
        }
        let picker = cx.debug_bounds("model-picker").expect("model picker");
        cx.simulate_click(picker.center(), gpui::Modifiers::default());
        draw(cx);
        draw(cx);
        let compact = shell.read_with(cx, |shell, cx| shell.compact(cx));
        assert_eq!(
            cx.debug_bounds("touch-picker-sheet").is_some(),
            compact,
            "mobile={mobile} width={width}: a compact window opens a sheet"
        );
        if !mobile {
            // The search has the keyboard: what is typed filters the list.
            assert!(
                cx.debug_bounds("model-fav-0").is_some(),
                "the picker lists the model"
            );
            cx.simulate_input("zzz");
            crate::test_frames::draw_settled(cx);
            assert!(
                cx.debug_bounds("model-fav-0").is_none(),
                "width={width}: off a phone the search raises the keyboard"
            );
            cx.simulate_keystrokes("backspace backspace backspace");
        }
        cx.simulate_keystrokes("escape");
        draw(cx);
        draw(cx);
        assert!(
            cx.debug_bounds("touch-picker-sheet").is_none(),
            "mobile={mobile} width={width}: Escape reaches the picker through its focus"
        );
    }
    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
}

#[gpui::test]
fn thread_shortcuts_follow_list_order_and_leave_model_picker_numbers_alone(
    cx: &mut TestAppContext,
) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let root = crate::store::tests::scratch_root("thread-shortcuts");
    let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
    let project = tcode_core::project::Project::from_root(root.join("project"));
    disk.upsert_project(&project).unwrap();
    for (id, updated_at) in [("third", 10), ("first", 30), ("second", 20)] {
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            project.root.clone(),
            None,
        );
        meta.id = id.into();
        meta.project_id = Some(project.id.clone());
        meta.updated_at = updated_at;
        disk.upsert_meta(&meta).unwrap();
    }
    let host = tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
        .unwrap();
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let (shell, _transport, cx) = mount(cx);
    cx.update(|_, cx| crate::shortcut::init(cx));
    cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| {
        *store = WorkspaceStore::new_attached(
            driver.link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        );
        store.show_thread("first".into(), cx);
    });
    driver.wait_until(cx, "selected thread baseline", |cx| {
        store.read_with(cx, |store, cx| {
            store.baseline_ready()
                && !store.threads_loading()
                && !store.chat_loading_for(Some("first"), cx)
        })
    });
    resize(cx, 1200.);
    draw(cx);

    let [first_key, second_key, out_of_range_key] = if cfg!(target_os = "macos") {
        ["cmd-1", "cmd-2", "cmd-9"]
    } else {
        ["ctrl-1", "ctrl-2", "ctrl-9"]
    };
    let picker = cx.debug_bounds("model-picker").expect("model picker");
    cx.simulate_click(picker.center(), gpui::Modifiers::default());
    draw(cx);
    cx.simulate_keystrokes(second_key);
    draw(cx);
    assert_eq!(
        shell
            .read_with(cx, |shell, cx| shell
                .page_thread(cx)
                .map(|thread| thread.session_id))
            .as_deref(),
        Some("first"),
        "model picker must keep number keys"
    );
    cx.simulate_keystrokes("escape");
    draw(cx);

    for (keys, expected) in [
        (second_key, "second"),
        ("ctrl-tab", "third"),
        ("ctrl-tab", "first"),
        ("ctrl-shift-tab", "third"),
        (out_of_range_key, "third"),
        (first_key, "first"),
    ] {
        cx.simulate_keystrokes(keys);
        driver.wait_until(cx, "selected thread baseline", |cx| {
            store.read_with(cx, |store, cx| {
                store.baseline_ready()
                    && !store.threads_loading()
                    && !store.chat_loading_for(Some(expected), cx)
            })
        });
        assert_eq!(
            shell
                .read_with(cx, |shell, cx| shell
                    .page_thread(cx)
                    .map(|thread| thread.session_id))
                .as_deref(),
            Some(expected),
            "{keys}"
        );
    }
    cx.update(|window, cx| window.blur(cx));
    draw(cx);
    cx.simulate_keystrokes(second_key);
    assert_eq!(
        shell
            .read_with(cx, |shell, cx| shell
                .page_thread(cx)
                .map(|thread| thread.session_id))
            .as_deref(),
        Some("second"),
        "shortcuts work without a focused input"
    );
    shell.update(cx, |shell, cx| shell.go(Destination::Settings, cx));
    draw(cx);
    cx.simulate_keystrokes(first_key);
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell.destination(cx)),
        Destination::Thread
    );
    resize(cx, 1200.);
    draw(cx);
    let row = cx
        .debug_bounds("sidebar-thread-second")
        .expect("second row");
    let settle = gpui::point(row.right() - px(18.), row.top() + px(14.));
    cx.simulate_mouse_move(settle, None, gpui::Modifiers::default());
    draw(cx);
    cx.simulate_click(settle, gpui::Modifiers::default());
    driver.wait_until(cx, "thread settlement update", |cx| {
        store.read_with(cx, |store, _| {
            store
                .sidebar_sessions()
                .iter()
                .any(|meta| meta.id == "second" && meta.is_settled())
        })
    });
    draw(cx);
    assert!(cx.update(|window, cx| window.has_notification::<crate::sidebar::ThreadUndo>(cx)));
    let undo_key = if cfg!(target_os = "macos") {
        "cmd-z"
    } else {
        "ctrl-z"
    };
    // Typing in the composer, the undo key is the field's.
    let card = cx.debug_bounds("composer-card").expect("the composer");
    cx.simulate_click(card.center(), gpui::Modifiers::default());
    draw(cx);
    cx.simulate_keystrokes("x");
    draw(cx);
    cx.simulate_keystrokes(undo_key);
    draw(cx);
    assert!(store.read_with(cx, |store, _| {
        store
            .sidebar_sessions()
            .iter()
            .any(|meta| meta.id == "second" && meta.is_settled())
    }));
    cx.update(|window, cx| window.blur(cx));
    draw(cx);
    cx.simulate_keystrokes(undo_key);
    driver.wait_until(cx, "thread settlement update", |cx| {
        store.read_with(cx, |store, _| {
            store
                .sidebar_sessions()
                .iter()
                .any(|meta| meta.id == "second" && !meta.is_settled())
        })
    });
    assert!(!cx.update(|window, cx| window.has_notification::<crate::sidebar::ThreadUndo>(cx)));
    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
}

/// A desktop tiled to half a screen is still the whole product: the width
/// rule belongs to mobile builds, so no desktop width is compact.
#[gpui::test]
fn a_narrow_desktop_window_keeps_the_wide_workspace(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount(cx);
    resize(cx, 400.);
    shell.read_with(cx, |shell, cx| {
        assert!(!shell.compact(cx), "400px on a desktop build is wide");
        assert_eq!(shell.window_state.read(cx).route(), Route::Chat);
    });
    assert!(
        cx.debug_bounds("sidebar-feature-hosts").is_some(),
        "400px keeps the workspace sidebar"
    );
    assert!(cx.debug_bounds("compact-threads-page").is_none());
}

/// Rotating a tablet across the breakpoint is a layout decision and
/// nothing else: it must not touch the attachment, the selected thread,
/// the draft or the split.
#[gpui::test]
fn a_tablet_turning_compact_keeps_its_thread_draft_link_and_split_width(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    as_mobile(cx);
    host.incoming
        .try_send(
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::Index,
                event: ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: Vec::new(),
                    projects: Vec::new(),
                }),
            }))
            .unwrap(),
        )
        .unwrap();
    cx.run_until_parked();
    // The thread is opened and the host describes it, so the chat draws
    // it with its composer.
    deliver_thread(&shell, &host, "thread-1", false, cx);

    resize(cx, 1024.);
    shell.read_with(cx, |shell, cx| {
        assert!(!shell.compact(cx), "1024px is the wide split");
    });
    draw(cx);
    draw(cx);
    let from = gpui::point(px(SIDEBAR_WIDTH), px(400.));
    let to = gpui::point(px(300.), px(400.));
    cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
    for step in [from + gpui::point(px(5.), px(0.)), to] {
        cx.simulate_mouse_move(step, Some(MouseButton::Left), gpui::Modifiers::default());
        draw(cx);
    }
    cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
    draw(cx);
    shell.read_with(cx, |shell, cx| {
        let attachment = shell.column.as_ref().unwrap();
        assert_eq!(attachment.split.read(cx).sizes()[0], px(300.));
        assert_eq!(attachment.sidebar_width.get(), px(300.));
    });
    let tap_composer = |cx: &mut VisualTestContext| {
        let card = cx.debug_bounds("composer-card").expect("the composer");
        cx.simulate_click(
            gpui::point(card.center().x, card.top() + px(24.)),
            gpui::Modifiers::default(),
        );
        draw(cx);
    };
    tap_composer(cx);
    cx.simulate_input("half a thought");
    draw(cx);
    let _ = sent(&host);

    resize(cx, 899.);
    shell.read_with(cx, |shell, cx| {
        assert!(shell.compact(cx), "899px is compact");
        assert_eq!(
            shell.destination(cx),
            Destination::Thread,
            "a selected thread is the compact destination"
        );
    });

    // The thread, the draft and the composer itself all survive.
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell
            .page_thread(cx)
            .map(|thread| thread.session_id)),
        Some("thread-1".into())
    );
    assert!(
        cx.debug_bounds("composer-settings-drawer").is_some(),
        "the composer followed the layout: a compact one carries the settings drawer"
    );

    // Nothing about the link changed: no shutdown, and no fresh index
    // subscription (which is what a reattach would look like).
    for payload in sent(&host) {
        assert!(
            !matches!(
                payload,
                ClientPayload::Command(Command::ShutdownAllAndFlush)
                    | ClientPayload::Subscribe(tcode_protocol::Subscription {
                        topic: Topic::Index,
                        ..
                    })
            ),
            "resizing must not detach or reconnect: {payload:?}"
        );
    }
    tap_composer(cx);
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(
        turns_sent(&host),
        [("thread-1".to_owned(), "half a thought".to_owned())],
        "the draft survives"
    );

    resize(cx, 1024.);
    draw(cx);
    draw(cx);
    shell.read_with(cx, |shell, cx| {
        assert!(!shell.compact(cx));
        assert_eq!(
            shell
                .column
                .as_ref()
                .expect("attached")
                .split
                .read(cx)
                .sizes()
                .first()
                .copied(),
            Some(px(300.)),
            "the wide split comes back at the width it was left at"
        );
    });
}

/// The one Back chain: overlays first, then one step of the window's
/// history, and `false` only at the root — where the platform closes the
/// app. Leaving a page never touches the attachment.
#[cfg(preview_backend)]
#[gpui::test]
fn compact_preview_release_and_navigation(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads", "thread"], true);
    let store = store_of(&shell, cx);
    restore_index(&shell, &host, true, cx);
    restore_status(&shell, &host, cx);
    await_restore_update(&shell, cx, |store, cx| {
        store.preview_key_for("thread-a", cx).is_some()
    });
    cx.simulate_resize(size(px(393.), px(852.)));
    draw(cx);
    let state = shell.read_with(cx, |shell, _| shell.window_state());
    let page_pane = shell.read_with(cx, |shell, _| {
        shell.column.as_ref().unwrap().current_pane.clone()
    });
    let key = store.read_with(cx, |store, cx| {
        store.preview_key_for("thread-a", cx).unwrap()
    });
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-a", tcode_core::ui::RightTab::Preview, cx);
    });
    render_preview_here(&host, "thread-a", cx);
    store.update(cx, |store, cx| {
        store.set_preview_url(&key, "https://example.com/".into(), cx);
    });
    state.update(cx, |state, cx| state.go(Destination::Panel, cx));
    draw(cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
    });
    draw(cx);
    assert_eq!(
        store
            .read_with(cx, |store, _| store.preview_url(&key))
            .as_deref(),
        Some("https://example.com/")
    );
    state.update(cx, |state, cx| state.go(Destination::Panel, cx));
    draw(cx);
    // Dispatch the same action as the compact overflow menu.
    cx.update(|window, cx| {
        let preview = page_pane.update(cx, |pane, cx| pane.drawer_preview(window, cx));
        preview.update(cx, |preview, cx| {
            preview.on_preview_action(&crate::preview_panel::PreviewAction::Close, window, cx)
        });
    });
    draw(cx);
    assert_eq!(
        store.read_with(cx, |store, _| store.preview_url(&key)),
        None
    );
    assert_eq!(
        state.read_with(cx, |state, _| state.destination()),
        Destination::Panel
    );
    store.update(cx, |store, cx| {
        store.set_preview_url(&key, "https://example.com/again".into(), cx)
    });
    cx.update(|window, cx| shell.update(cx, |shell, cx| assert!(shell.back(window, cx))));
    draw(cx);
    cx.update(|window, cx| shell.update(cx, |shell, cx| assert!(shell.back(window, cx))));
    draw(cx);
    assert_eq!(
        state.read_with(cx, |state, _| state.destination()),
        Destination::Threads
    );
    assert_eq!(
        store.read_with(cx, |store, _| store.preview_url(&key)),
        None
    );
}

#[gpui::test]
fn back_unwinds_overlays_then_the_history_and_stops_at_the_root(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount(cx);
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    as_mobile(cx);
    resize(cx, 393.);

    let back = |cx: &mut VisualTestContext| {
        let consumed = cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx)));
        draw(cx);
        consumed
    };

    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.destination(cx), Destination::Thread)
    });

    // An open overlay outranks navigation.
    let window_state = shell.read_with(cx, |shell, _| shell.window_state());
    window_state.update(cx, |state, cx| state.open_palette(cx));
    assert!(back(cx), "back closes the overlay");
    shell.read_with(cx, |shell, cx| {
        assert!(!shell.window_state.read(cx).palette_open);
        assert_eq!(
            shell.destination(cx),
            Destination::Thread,
            "and nothing else"
        );
    });

    // Thread → Threads, the root. Neither leaving the thread nor a visit
    // to Machines leaves the host: only an explicit Disconnect does.
    assert!(back(cx));
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.destination(cx), Destination::Threads);
        assert!(shell.column.is_some(), "still attached");
    });
    assert!(!back(cx), "the root belongs to the platform");

    window_state.update(cx, |state, cx| {
        state.go(Destination::Hosts, cx);
        state.go(Destination::Pair, cx);
    });
    draw(cx);
    assert!(back(cx));
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell.destination(cx)),
        Destination::Hosts
    );
    assert!(back(cx));
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.destination(cx), Destination::Threads);
        assert!(
            shell.column.is_some(),
            "walking back from the hosts list is not a decision to leave the host"
        );
    });
    assert!(!back(cx), "the root belongs to the platform");
}

/// Settings is two pages of that same history, not a route with a back row
/// of its own: detail → root → whatever Settings was opened from.
#[gpui::test]
fn back_leaves_a_settings_section_then_settings_then_the_page_below(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount(cx);
    as_mobile(cx);
    resize(cx, 393.);
    let window_state = shell.read_with(cx, |shell, _| shell.window_state());

    window_state.update(cx, |state, cx| state.open_settings(cx));
    // The page is tapped once it has slid in.
    crate::test_frames::draw_settled(cx);
    // One host: the machine's group is captioned, and chooses nothing.
    assert!(cx.debug_bounds("settings-machine-caption").is_some());
    assert!(cx.debug_bounds("settings-host-selector").is_none());
    let general = cx
        .debug_bounds("settings-nav-general")
        .expect("General settings row");
    cx.simulate_click(general.center(), gpui::Modifiers::default());
    draw(cx);
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.destination(cx), Destination::SettingsSection)
    });

    let back = |cx: &mut VisualTestContext| {
        let consumed = cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx)));
        draw(cx);
        consumed
    };
    assert!(back(cx));
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.destination(cx), Destination::Settings)
    });
    assert!(back(cx));
    shell.read_with(cx, |shell, cx| {
        assert_eq!(
            shell.destination(cx),
            Destination::Threads,
            "settings returns to the page it was opened from"
        );
    });
}

/// Wide, the pairing page is pushed over Machines, and its header's Back
/// returns to Machines rather than leaving the Hosts route; Machines, a
/// root the sidebar switches to, has no Back. What was pasted there, and
/// the error it drew, stay behind on the page that was left.
#[gpui::test]
fn the_wide_pair_page_goes_back_to_machines(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount(cx);
    resize(cx, 1024.);
    let state = shell.read_with(cx, |shell, _| shell.window_state());
    state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
    draw(cx);
    assert!(
        cx.debug_bounds("hosts-back").is_none(),
        "Machines is a root"
    );
    let paste = cx
        .debug_bounds("hosts-pair")
        .expect("Machines offers pasting an invitation");
    cx.simulate_click(paste.center(), gpui::Modifiers::default());
    draw(cx);
    cx.simulate_input("not an invitation");
    draw(cx);
    assert!(
        cx.debug_bounds("hosts-pair-error").is_some(),
        "the paste page flags what is not an invitation"
    );
    // Back widens in with the pairing page; it is pressed once there.
    crate::test_frames::draw_settled(cx);
    let back = cx
        .debug_bounds("hosts-back")
        .expect("the pairing page offers Back");
    cx.simulate_click(back.center(), gpui::Modifiers::default());
    // The pairing page fades out over Machines, drawn as it was.
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell.destination(cx)),
        Destination::Hosts
    );
    assert!(cx.debug_bounds("hosts-route").is_some());
    assert!(
        cx.debug_bounds("hosts-pair-error").is_none(),
        "the error does not follow the user back to Machines"
    );
}

/// A store selection can come from the sidebar or palette. In wide layout
/// it switches the content column away from Hosts without requiring Back.
#[gpui::test]
fn wide_hosts_route_keeps_its_sidebar_until_a_thread_selection(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, _host, cx) = mount(cx);
    resize(cx, 1024.);
    draw(cx);
    for thread in ["thread-1", "thread-2"] {
        let feature = cx
            .debug_bounds("sidebar-feature-hosts")
            .expect("the Machines feature row is visible");
        cx.simulate_mouse_down(
            feature.center(),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.simulate_mouse_up(
            feature.center(),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        draw(cx);
        assert!(cx.debug_bounds("hosts-route").is_some());
        assert!(cx.debug_bounds("sidebar-feature-hosts").is_some());
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.window_state.read(cx).route()),
            Route::Hosts
        );

        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            store.show_thread(thread.into(), cx);
        });
        crate::test_frames::draw_settled(cx);

        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.window_state.read(cx).route(), Route::Chat);
        });
        assert!(
            cx.debug_bounds("hosts-route").is_none(),
            "the chat content replaces Machines as soon as the store selection changes"
        );
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell
                .window_state
                .read(cx)
                .history()
                .to_vec()),
            vec![Destination::Threads, Destination::Thread]
        );
    }
    let state = shell.read_with(cx, |shell, _| shell.window_state());
    state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
    draw(cx);
    assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
    crate::test_frames::draw_settled(cx);
    assert_eq!(state.read_with(cx, |state, _| state.route()), Route::Chat);
    assert!(!cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));

    // The control toggles: on Machines, or on Pair over it, a click returns
    // to the thread the window was showing.
    let click_control = |cx: &mut VisualTestContext| {
        let control = cx
            .debug_bounds("sidebar-feature-hosts")
            .expect("the Machines control is visible");
        cx.simulate_click(control.center(), gpui::Modifiers::default());
        crate::test_frames::draw_settled(cx);
    };
    for pairing in [false, true] {
        click_control(cx);
        assert_eq!(state.read_with(cx, |state, _| state.route()), Route::Hosts);
        if pairing {
            state.update(cx, |state, cx| state.go(Destination::Pair, cx));
            draw(cx);
        }
        click_control(cx);
        assert_eq!(
            state.read_with(cx, |state, _| state.history().to_vec()),
            vec![Destination::Threads, Destination::Thread],
            "pairing: {pairing}"
        );
        assert!(cx.debug_bounds("hosts-route").is_none());
    }
}

/// Opening the first host replaces the empty threads with its workspace;
/// leaving the last brings the empty threads back, and the workspace's
/// views are let go.
#[gpui::test]
fn the_first_host_and_the_last_replace_what_the_window_showed(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let studio = saved_machine("studio", false);
    let (shell, _hosts, _client, cx) = mount_workspace(
        cx,
        vec![studio.clone()],
        tcode_client::host::ClientPreferences {
            workspace: Some(Default::default()),
            ..Default::default()
        },
        1024.,
    );
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("no-hosts").is_some());

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_host(AttachmentTarget::Remote(Box::new(studio)), window, cx)
        })
    });
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("chat-route").is_some(), "the workspace");
    assert!(cx.debug_bounds("no-hosts").is_none());

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.leave_host(&HostKey::Remote("studio".into()), window, cx)
        })
    });
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("no-hosts").is_some(), "the empty threads");
    assert!(cx.debug_bounds("chat-route").is_none());
}

/// Settings replaces the whole window, unlike the Hosts route: the left
/// column is the settings rail, so the workspace sidebar — and the feature
/// area in it — is gone while the route is showing.
#[gpui::test]
fn the_wide_settings_route_replaces_the_sidebar_with_its_own_rail(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount(cx);
    resize(cx, 1024.);
    let window_state = shell.read_with(cx, |shell, _| shell.window_state());
    window_state.update(cx, |state, cx| state.open_settings(cx));
    crate::test_frames::draw_settled(cx);
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.window_state.read(cx).route(), Route::Settings);
    });
    assert!(
        cx.debug_bounds("settings-route").is_some(),
        "settings fills the window"
    );
    assert!(
        cx.debug_bounds("sidebar-feature-hosts").is_none(),
        "and the workspace sidebar is not beside it"
    );
    assert!(
        cx.debug_bounds("settings-device-caption").is_some(),
        "the rail carries the settings group captions instead"
    );
    assert!(
        cx.debug_bounds("settings-nav-threads").is_some(),
        "thread settings are a rail page of their own, as on a phone"
    );
    let general = cx
        .debug_bounds("settings-nav-general")
        .expect("settings navigation");
    cx.simulate_click(general.center(), gpui::Modifiers::default());
    draw(cx);
    assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
    draw(cx);
    assert_eq!(
        window_state.read_with(cx, |state, _| state.route()),
        Route::Chat
    );
    assert!(cx.debug_bounds("sidebar-feature-hosts").is_some());
    assert!(!cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
}

/// Visiting Hosts from an open thread is navigation, not a reconnection:
/// the link, the selected thread and the views over it all stay put, and
/// Back returns to exactly where the visit started.
#[gpui::test]
fn a_hosts_visit_keeps_the_link_and_returns_to_the_thread(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    as_mobile(cx);
    resize(cx, 393.);
    let _ = sent(&host);

    let window_state = shell.read_with(cx, |shell, _| shell.window_state());
    window_state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
    draw(cx);
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.destination(cx), Destination::Hosts);
        assert!(shell.column.is_some(), "the visit keeps the attachment");
    });
    for payload in sent(&host) {
        assert!(
            !matches!(
                payload,
                ClientPayload::Command(Command::ShutdownAllAndFlush)
                    | ClientPayload::Subscribe(tcode_protocol::Subscription {
                        topic: Topic::Index,
                        ..
                    })
            ),
            "visiting hosts must not detach or reconnect: {payload:?}"
        );
    }

    assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
    draw(cx);
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.destination(cx), Destination::Thread);
    });
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell
            .page_thread(cx)
            .map(|thread| thread.session_id)),
        Some("thread-1".into())
    );
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

#[gpui::test]
fn switching_language_retitles_the_window(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
    let (shell, host, cx) = mount(cx);
    let session = "draft-1";
    let mut status = session_status(session, std::path::Path::new("/tmp/tcode-title"));
    status.draft = true;
    host.incoming
        .try_send(
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: session.into(),
                },
                event: ServerEvent::SessionStatusReplaced(Box::new(status)),
            }))
            .unwrap(),
        )
        .unwrap();
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| store.show_thread(session.into(), cx));
    cx.run_until_parked();
    cx.run_until_parked();
    crate::settings::assert_follows_language_switch(cx, "chat.new_thread", |cx| {
        cx.window_title().expect("the shell titles its window")
    });
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

/// The compact Panel page owns the chrome: its segmented control is the only
/// selector, so the diff panel's own tab row and its right-panel window
/// controls are not built at all — and the page holds its 16pt inset even
/// when the diff line is far wider than the window.
#[gpui::test]
fn the_compact_panel_page_leaves_out_the_right_panels_tabs_and_close(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    seed_wide_diff(&shell, &host, cx);
    resize(cx, 1024.);
    store_of(&shell, cx).update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
    });
    draw_until(cx, &host, "diff-body", None);
    assert!(cx.debug_bounds("right-panel-tabs").is_some());
    assert!(cx.debug_bounds("diff-close").is_some());
    as_mobile(cx);
    resize(cx, 393.);
    shell.update(cx, |shell, cx| shell.open_panels(cx));
    draw_until(cx, &host, "diff-body", None);
    crate::test_frames::draw_settled(cx);

    assert!(
        cx.debug_bounds("right-panel-tabs").is_none(),
        "the Diff | Tasks tab row is the wide layout's selector"
    );
    assert!(
        cx.debug_bounds("diff-close").is_none(),
        "close is a right-panel affordance; Back leaves the compact page"
    );
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

/// A reader halfway up a long conversation keeps their place as the
/// right panel opens beside it and the conversation narrows: the
/// message the timeline starts at is the one it started at before.
#[gpui::test]
fn opening_the_right_panel_keeps_the_reader_at_the_message_they_were_at(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(crate::markdown::init);
    cx.update(|cx| cx.set_reduce_motion(true));
    let root = crate::store::tests::scratch_root("panel-reader");
    let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
    let mut meta =
        tcode_core::project::SessionMeta::new(agent::ProviderKind::ClaudeCode, root.clone(), None);
    meta.id = "thread-1".into();
    disk.upsert_meta(&meta).unwrap();
    // Answers long enough to wrap again when the conversation narrows.
    let answer =
        "Each step of the migration checks what the one before it left behind. ".repeat(12);
    let message = |id: String, content: agent::ItemContent| {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id,
            parent_item_id: None,
            content,
        })
    };
    let log = (0..40).flat_map(|turn| {
        [
            agent::AgentEvent::TurnStarted {
                turn_id: turn.to_string(),
            },
            message(
                format!("user-{turn}"),
                agent::ItemContent::UserMessage {
                    text: format!("Question {turn}"),
                    attachments: Vec::new(),
                },
            ),
            message(
                format!("answer-{turn}"),
                agent::ItemContent::AssistantMessage {
                    text: answer.clone(),
                    summarized: false,
                },
            ),
            agent::AgentEvent::TurnCompleted {
                turn_id: turn.to_string(),
                status: agent::TurnStatus::Completed,
                usage: None,
            },
        ]
    });
    for (ts, event) in log.enumerate() {
        disk.append_event(&meta.id, ts as u64, &event).unwrap();
    }
    let (_shell, store, host, mut driver, cx) = mount_on_host(cx, disk, None);
    resize(cx, 1100.);
    // The reader goes to a turn halfway up, as the palette takes them.
    store.update(cx, |store, cx| {
        store.show_thread_at_turn("thread-1".into(), 20, cx)
    });
    driver.settle_until(cx, "the turn the reader went to", |cx| {
        cx.update(|_, cx| store.read(cx).pending_chat_turn("thread-1").is_none())
            && top_row(cx).is_some_and(|row| row > 0)
    });
    // And scrolls up a little, into the middle of a message.
    let timeline = cx.debug_bounds("chat-timeline").unwrap();
    for (delta, phase) in [
        (0., gpui::TouchPhase::Started),
        (90., gpui::TouchPhase::Moved),
        (0., gpui::TouchPhase::Ended),
    ] {
        cx.simulate_event(gpui::ScrollEvent {
            position: timeline.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(delta))),
            touch_phase: phase,
            ..Default::default()
        });
        crate::test_frames::draw_settled(cx);
    }
    let reading = top_row(cx).expect("the timeline shows the thread");

    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Plan, cx)
    });
    crate::test_frames::draw_settled(cx);
    assert!(
        cx.debug_bounds("right-panel").is_some(),
        "the panel is open"
    );
    assert_eq!(top_row(cx), Some(reading), "the reader is where they were");

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
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

/// The right panel comes back to the width the user chose by dragging
/// its edge after the window narrows and widens again, after a sidebar
/// drag, and after it was expanded to everything beside the sidebar and
/// dragged there. In a narrow window the close button closes the panel.
#[gpui::test]
fn the_right_panel_keeps_the_width_the_user_dragged_it_to(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_thread_with(cx, Vec::new());
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    resize(cx, 1600.);
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
    });
    draw_until(cx, &host, "diff-body", None);

    let width_at = |cx: &mut VisualTestContext, width: f32| {
        resize(cx, width);
        draw_until(cx, &host, "diff-body", None);
        crate::test_frames::draw_settled(cx);
        cx.debug_bounds("right-panel")
            .expect("the right panel")
            .size
            .width
    };
    let drag = |cx: &mut VisualTestContext, from: gpui::Point<Pixels>, by: Pixels| {
        let to = from + gpui::point(by, px(0.));
        // As a pointer does: the drag starts on the first move and moves
        // the handle on the next, and the release listener exists only
        // while a resize is under way, so each step gets its frame.
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        for step in [from + gpui::point(px(5.), px(0.)), to] {
            cx.simulate_mouse_move(step, Some(MouseButton::Left), gpui::Modifiers::default());
            draw(cx);
        }
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
    };

    // A drag of the panel's own edge chooses its width, which a narrower
    // and a wider window keep.
    width_at(cx, 1600.);
    let panel = cx.debug_bounds("right-panel").unwrap();
    drag(cx, gpui::point(panel.left(), px(400.)), px(60.));
    let chosen = cx.debug_bounds("right-panel").unwrap().size.width;
    assert!(chosen < panel.size.width, "the edge shrinks the panel");
    width_at(cx, 900.);
    assert_eq!(width_at(cx, 1600.), chosen, "after the window narrowed");
}

/// A docked right panel leaves the keyboard where it was as it opens. A
/// press in the panel takes it there, a press on the chat beside it takes
/// it back with the panel left open, and closing the panel from inside
/// gives it back to what had it.
#[gpui::test]
fn the_right_panel_gives_the_keyboard_back(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    resize(cx, 1600.);
    let press_composer = |cx: &mut VisualTestContext| {
        let composer = cx.debug_bounds("chat-composer").unwrap();
        cx.simulate_click(
            gpui::point(composer.left() + px(40.), composer.top() + px(24.)),
            gpui::Modifiers::default(),
        );
        draw(cx);
    };
    let open = |cx: &mut VisualTestContext| {
        press_composer(cx);
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
        });
        crate::test_frames::draw_settled(cx);
        assert_eq!(typed_reaches(&host, cx), ["thread-1"], "the keyboard stays");
    };
    let press_panel = |cx: &mut VisualTestContext| {
        let panel = cx.debug_bounds("right-panel").unwrap();
        cx.simulate_click(panel.center(), gpui::Modifiers::default());
        draw(cx);
        assert_eq!(
            typed_reaches(&host, cx),
            Vec::<String>::new(),
            "the press takes it"
        );
    };

    open(cx);
    press_panel(cx);
    press_composer(cx);
    assert_eq!(
        typed_reaches(&host, cx),
        ["thread-1"],
        "the press reaches the chat"
    );
    assert!(store.read_with(cx, |store, _| {
        store.panel_state_for(Some("thread-1")).right_panel_open
    }));
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
    });
    crate::test_frames::draw_settled(cx);

    open(cx);
    press_panel(cx);
    let close = cx.debug_bounds("diff-close").unwrap();
    cx.simulate_click(close.center(), gpui::Modifiers::default());
    draw(cx);
    assert_eq!(
        typed_reaches(&host, cx),
        ["thread-1"],
        "closing returns to the composer"
    );
}

#[gpui::test]
fn a_narrow_chat_headers_overflow_menu_opens_the_diff(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_crowded_thread(cx);
    let store = store_of(&shell, cx);
    // Narrower than the header at its narrowest.
    resize(cx, SIDEBAR_WIDTH + crate::chat::HEADER_OVERFLOW_MIN_WIDTH);
    // The header glides into its overflow tier before a reader can
    // reach it.
    crate::test_frames::draw_settled(cx);
    let header = cx.debug_bounds("chat-header-scroll").unwrap();
    cx.simulate_event(gpui::ScrollEvent {
        position: header.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(-1000.), px(0.))),
        ..Default::default()
    });
    draw(cx);
    let toggle = cx.debug_bounds("header-overflow").unwrap();
    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    draw(cx);
    draw(cx);
    // Terminal, tasks, then the diff.
    let diff = cx.debug_bounds("menu-item-2").expect("the diff's row");
    cx.simulate_click(diff.center(), gpui::Modifiers::default());
    draw_until(cx, &host, "diff-body", None);
    assert!(store.read_with(cx, |store, _| {
        store.panel_state_for(Some("thread-1")).right_panel_open
    }));
}

/// The window growing wider leaves the terminal drawer's height as the
/// thread holds it: the host is told a height only when it changes.
#[gpui::test]
fn a_wider_window_sends_the_host_no_terminal_height(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_thread_with(cx, Vec::new());
    let store = store_of(&shell, cx);
    resize(cx, 1600.);
    store.update(cx, |store, cx| {
        store.toggle_terminal_panel_for("thread-1".to_owned(), cx)
    });
    crate::test_frames::draw_settled(cx);
    let _ = sent(&host);

    resize(cx, 1800.);
    crate::test_frames::draw_settled(cx);
    let heights: Vec<_> = sent(&host)
        .into_iter()
        .filter(|payload| {
            matches!(
                payload,
                ClientPayload::Command(Command::SetTerminalHeight { .. })
            )
        })
        .collect();
    assert!(heights.is_empty(), "{heights:?}");
}

/// A phone opening the terminal page tells the host no drawer height:
/// the page has no drawer, and the height is the desktop's.
#[gpui::test]
fn the_compact_terminal_page_sends_the_host_no_terminal_height(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_thread_with(cx, Vec::new());
    as_mobile(cx);
    resize(cx, 393.);
    let _ = sent(&host);

    shell.update(cx, |shell, cx| {
        shell.show_panel("terminal", cx);
        shell.go(Destination::Panel, cx);
    });
    crate::test_frames::draw_settled(cx);
    let heights: Vec<_> = sent(&host)
        .into_iter()
        .filter(|payload| {
            matches!(
                payload,
                ClientPayload::Command(Command::SetTerminalHeight { .. })
            )
        })
        .collect();
    assert!(heights.is_empty(), "{heights:?}");
}

/// The working tree changes outside any turn — the user edits a file while
/// another tab is showing. Choosing the Diff tab reads the tree again, as
/// opening the panel does, and a tree with nothing to show draws the empty
/// state rather than an empty list.
#[gpui::test]
fn diff_tab_reads_the_working_tree_again_when_chosen(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    open_working_tree_panel(&shell, &host, cx);
    let store = store_of(&shell, cx);
    let clean = tcode_protocol::GitDiffResult::default();
    assert_eq!(draw_until(cx, &host, "diff-empty", Some(&clean)), 1);
    assert!(
        cx.debug_bounds("diff-body").is_none(),
        "a clean tree draws the empty state, not an empty list"
    );

    store.update(cx, |store, cx| {
        store.set_right_tab_for(Some("thread-1"), RightTab::PullRequests, cx)
    });
    draw(cx);
    let changed = tcode_protocol::GitDiffResult {
        changes: vec![agent::FileChange {
            path: "README.md".into(),
            kind: agent::FileChangeKind::Modify,
            diff: Some("@@ -1 +1,2 @@\n # acme-api\n+Checkout service.\n".into()),
        }],
        texts: vec![tcode_protocol::GitFileText {
            old: Some("# acme-api\n".into()),
            new: Some("# acme-api\nCheckout service.\n".into()),
        }],
        ..Default::default()
    };
    store.update(cx, |store, cx| {
        store.set_right_tab_for(Some("thread-1"), RightTab::Diff, cx)
    });
    assert_eq!(
        draw_until(cx, &host, "diff-body", Some(&changed)),
        1,
        "choosing the Diff tab reads the working tree again and lists the changed file"
    );
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

/// While an agent works the tree changes under the reader. Reading it again leaves the
/// diff they were reading on screen — not a loading state — until the new one is built.
#[gpui::test]
fn working_tree_refresh_keeps_the_previous_diff_until_the_new_one_lands(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    open_working_tree_panel(&shell, &host, cx);
    let reads = held_git_diff_reads(&host, cx);
    assert_eq!(reads.len(), 1, "opening the panel reads the working tree");
    answer_git_diff(&host, reads[0], &created_files(&[("first.rs", 3)]));
    assert!(held_git_diff_reads(&host, cx).is_empty());
    assert!(cx.debug_bounds("diff-line-new-first.rs-1").is_some());

    complete_turn(&host);
    let reads = held_git_diff_reads(&host, cx);
    assert_eq!(reads.len(), 1, "the finished turn reads the tree again");
    assert!(
        cx.debug_bounds("diff-line-new-first.rs-1").is_some(),
        "the previous diff stays on screen while the host reads the tree"
    );

    answer_git_diff(
        &host,
        reads[0],
        &created_files(&[("first.rs", 3), ("second.rs", 2)]),
    );
    held_git_diff_reads(&host, cx);
    assert!(
        cx.debug_bounds("diff-line-new-second.rs-1").is_some(),
        "the new diff replaces it once built"
    );
}

/// Swapping in the new diff keeps the reader on the line they were reading, even when a
/// file now listed above it pushes every row down.
#[gpui::test]
fn working_tree_refresh_keeps_the_reading_position(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    open_working_tree_panel(&shell, &host, cx);
    let reads = held_git_diff_reads(&host, cx);
    answer_git_diff(&host, reads[0], &created_files(&[("long.rs", 100)]));
    held_git_diff_reads(&host, cx);

    let body = cx
        .debug_bounds("diff-body")
        .expect("the diff body laid out");
    // The reader scrolls into the file, a wheel turn at a time.
    for _ in 0..12 {
        cx.simulate_event(gpui::ScrollEvent {
            position: body.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-100.))),
            ..Default::default()
        });
        draw(cx);
    }
    crate::test_frames::draw_settled(cx);
    // Debug selectors are looked up by `&'static str`.
    let selector = |line: u32| -> &'static str { format!("diff-line-new-long.rs-{line}").leak() };
    let top_line = |cx: &mut VisualTestContext| {
        (1..=100).find_map(|line| {
            cx.debug_bounds(selector(line))
                .filter(|bounds| bounds.top() >= body.top())
                .map(|bounds| (line, bounds.top()))
        })
    };
    let (line, top) = top_line(cx).expect("a line of long.rs is on screen");
    assert!(line > 20, "the reader scrolled into the file: {line}");

    complete_turn(&host);
    let reads = held_git_diff_reads(&host, cx);
    answer_git_diff(
        &host,
        reads[0],
        &created_files(&[("added.rs", 20), ("long.rs", 100)]),
    );
    held_git_diff_reads(&host, cx);
    assert!(
        cx.debug_bounds("diff-line-new-added.rs-1").is_none(),
        "the file listed above is out of view"
    );
    let after = cx
        .debug_bounds(selector(line))
        .expect("the line read before the refresh is still on screen");
    assert!(
        (after.top() - top).abs() < px(1.),
        "it stays where it was: {top:?} then {:?}",
        after.top()
    );
}

/// Reads answered out of order: an earlier read the host finishes last is superseded and
/// never replaces the newer diff, nor what was shown while both were pending.
#[gpui::test]
fn working_tree_refresh_ignores_a_superseded_read(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    open_working_tree_panel(&shell, &host, cx);
    let reads = held_git_diff_reads(&host, cx);
    answer_git_diff(&host, reads[0], &created_files(&[("first.rs", 3)]));
    held_git_diff_reads(&host, cx);

    complete_turn(&host);
    let earlier = held_git_diff_reads(&host, cx);
    complete_turn(&host);
    let later = held_git_diff_reads(&host, cx);
    assert_eq!((earlier.len(), later.len()), (1, 1));

    answer_git_diff(&host, earlier[0], &created_files(&[("stale.rs", 3)]));
    held_git_diff_reads(&host, cx);
    assert!(cx.debug_bounds("diff-line-new-stale.rs-1").is_none());
    assert!(
        cx.debug_bounds("diff-line-new-first.rs-1").is_some(),
        "a superseded read leaves the shown diff in place"
    );

    answer_git_diff(&host, later[0], &created_files(&[("newest.rs", 3)]));
    held_git_diff_reads(&host, cx);
    assert!(cx.debug_bounds("diff-line-new-newest.rs-1").is_some());
    assert!(cx.debug_bounds("diff-line-new-stale.rs-1").is_none());
}

#[gpui::test]
fn a_collapsed_sidebar_stays_available_through_its_interactions(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (_shell, _host, cx) = mount_panes(cx);
    let none = gpui::Modifiers::default();
    let edge = gpui::point(px(4.), px(400.));
    let overlay = |cx: &mut VisualTestContext| cx.debug_bounds("sidebar-hover-overlay");

    cx.simulate_mouse_move(edge, None, none);
    crate::test_frames::draw_settled(cx);
    assert_eq!(overlay(cx), None, "the sidebar is in its column");

    cx.simulate_mouse_move(gpui::point(px(1000.), px(400.)), None, none);
    cx.update(|_, cx| crate::device::DevicePreferences::toggle_sidebar_collapsed(cx));
    crate::test_frames::draw_settled(cx);
    assert_eq!(overlay(cx), None);

    cx.simulate_mouse_move(edge, None, none);
    crate::test_frames::draw_settled(cx);
    let shown = overlay(cx).expect("the edge brings the sidebar in");
    cx.simulate_mouse_move(shown.center(), None, none);
    crate::test_frames::draw_settled(cx);
    assert!(overlay(cx).is_some(), "it stays while the pointer is on it");

    cx.simulate_event(gpui::ScrollEvent {
        position: shown.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-30.))),
        ..Default::default()
    });
    crate::test_frames::draw_settled(cx);
    assert!(
        overlay(cx).is_some(),
        "scrolling keeps the sidebar available"
    );

    cx.simulate_keystrokes("tab");
    crate::test_frames::draw_settled(cx);
    assert!(
        overlay(cx).is_some(),
        "keyboard navigation keeps the sidebar available"
    );
    let row = cx
        .debug_bounds("sidebar-thread-thread-1")
        .expect("a draggable thread");
    cx.simulate_mouse_move(row.center(), None, none);
    cx.simulate_mouse_down(row.center(), MouseButton::Left, none);
    let outside = gpui::point(shown.right() + px(200.), px(400.));
    cx.simulate_mouse_move(outside, Some(MouseButton::Left), none);
    crate::test_frames::draw_settled(cx);
    assert!(
        overlay(cx).is_some(),
        "the sidebar owns its drag until release"
    );
    cx.simulate_mouse_up(outside, MouseButton::Left, none);
    crate::test_frames::draw_settled(cx);
    assert_eq!(overlay(cx), None, "releasing outside ends the interaction");
    cx.simulate_mouse_move(edge, None, none);
    crate::test_frames::draw_settled(cx);

    cx.simulate_mouse_move(gpui::point(shown.right() + px(200.), px(400.)), None, none);
    crate::test_frames::draw_settled(cx);
    assert_eq!(overlay(cx), None, "it goes once the pointer leaves it");

    cx.simulate_mouse_move(edge, None, none);
    crate::test_frames::draw_settled(cx);
    let row = cx.debug_bounds("sidebar-thread-thread-1").unwrap();
    cx.simulate_mouse_down(row.center(), MouseButton::Right, none);
    cx.simulate_mouse_up(row.center(), MouseButton::Right, none);
    crate::test_frames::draw_settled(cx);
    assert!(
        cx.debug_bounds("tcode-popup-menu").is_some(),
        "the thread menu opens"
    );
    cx.simulate_mouse_move(outside, None, none);
    crate::test_frames::draw_settled(cx);
    assert!(overlay(cx).is_some(), "the sidebar owns its focused menu");
    cx.simulate_keystrokes("escape");
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        overlay(cx),
        None,
        "dismissing the menu outside releases the sidebar"
    );
    cx.simulate_event(gpui::FileDropEvent::Entered {
        position: edge,
        paths: gpui::ExternalPaths(vec![std::path::PathBuf::from("/tmp/sidebar-drag.txt")].into()),
    });
    crate::test_frames::draw_settled(cx);
    assert!(
        overlay(cx).is_some(),
        "dragging to the edge opens the sidebar"
    );
    cx.simulate_event(gpui::FileDropEvent::Exited);
    crate::test_frames::draw_settled(cx);
    assert_eq!(overlay(cx), None, "leaving the window releases the sidebar");
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

/// A control the keyboard is on keeps it when its view is drawn under a
/// new parent: a command's header in the left of two panes, as the pane
/// is zoomed over the column, back, and as the tablet window turns
/// compact. The turn runs, so its working indicator would keep the
/// window drawing; with motion reduced it rests.
#[gpui::test]
fn a_focused_control_keeps_the_keyboard_through_a_zoom_and_a_compact_flip(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    serve_running_command(&host);
    crate::test_frames::draw_settled(cx);
    // Shut, the command's header is the timeline's last control.
    click_command(cx);
    cx.simulate_keystrokes("secondary-d");
    crate::test_frames::draw_settled(cx);
    cx.simulate_keystrokes("secondary-alt-left");
    crate::test_frames::draw_settled(cx);
    let _ = sent(&host);
    // Back from the composer to the header, as Shift-Tab takes it.
    cx.update(|window, cx| window.focus_prev(cx));
    cx.simulate_keystrokes("shift");
    crate::test_frames::draw_settled(cx);
    let focused = cx
        .update(|window, cx| window.focused(cx))
        .expect("the header holds the keyboard");
    assert_eq!(keyboard_pane(&shell, cx), Some(0));
    let still = |what: &str, cx: &mut VisualTestContext| {
        assert_eq!(
            cx.update(|window, cx| window.focused(cx)).as_ref(),
            Some(&focused),
            "{what}: the header lost the keyboard"
        );
    };

    cx.simulate_keystrokes("secondary-shift-enter");
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("zoomed-pane").is_some());
    still("zoomed", cx);
    cx.simulate_keystrokes("secondary-shift-enter");
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("zoomed-pane").is_none());
    still("back in its slot", cx);

    // A tablet's window, at a width that turns compact.
    as_mobile(cx);
    cx.simulate_resize(size(px(899.), px(800.)));
    crate::test_frames::draw_settled(cx);
    shell.read_with(cx, |shell, cx| assert!(shell.compact(cx)));
    still("compact", cx);
}
