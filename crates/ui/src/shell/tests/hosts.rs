use super::*;

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
    cx.simulate_click(
        gpui::point(card.center().x, card.top() + px(24.)),
        gpui::Modifiers::default(),
    );
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
    cx.simulate_click(
        gpui::point(card.center().x, card.top() + px(24.)),
        gpui::Modifiers::default(),
    );
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

/// Pairing switches the window to the newly paired target's scope: pairing
/// into a machine group opens that group's members without mixing in a
/// previously open standalone machine, pairing a standalone machine opens it
/// alone, and pairing a space opens that space alone.
#[gpui::test]
fn pairing_switches_to_the_paired_group_standalone_or_space(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, client, cx) = mount_workspace(
        cx,
        vec![saved_machine("solo", false)],
        Default::default(),
        1200.,
    );
    let key_solo = HostKey::Remote("solo".into());
    let solo = store_on(&shell, "solo", cx);
    hosts[&key_solo].come_up(&solo, Vec::new(), None, cx);

    // Pairing with a machine in a group (which also saves its peer in the
    // roster) opens only that group, never mixing in `solo`.
    let _b = WorkspaceHost::offer(&client, "host-b", cx);
    let _c = WorkspaceHost::offer(&client, "host-c", cx);
    client.remember_host(saved_machine("host-c", true)).unwrap();
    pair_with(&shell, &client, saved_machine("host-b", true), cx);
    assert_eq!(
        linked(&shell, cx),
        [
            HostKey::Remote("host-c".into()),
            HostKey::Remote("host-b".into()),
        ]
    );
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell.window_state.read(cx).route()),
        Route::Chat
    );

    // Pairing a standalone machine switches to that machine alone.
    let _d = WorkspaceHost::offer(&client, "host-d", cx);
    pair_with(&shell, &client, saved_machine("host-d", false), cx);
    assert_eq!(linked(&shell, cx), [HostKey::Remote("host-d".into())]);

    // Pairing a space switches to that space alone.
    let _e = WorkspaceHost::offer(&client, "host-e", cx);
    pair_with(
        &shell,
        &client,
        tcode_client::pairing::PairedHost {
            space_id: Some("space".into()),
            space_name: Some("Shared".into()),
            ..saved_machine("host-e", false)
        },
        cx,
    );
    assert_eq!(linked(&shell, cx), [HostKey::Remote("host-e".into())]);
}

/// Saving a new group member never invades a standalone machine's view; when
/// the window is viewing that group, a newly learned member of the same group
/// opens immediately beside the existing ones.
#[gpui::test]
fn group_roster_updates_stay_out_of_standalone_views_and_open_in_their_group(
    cx: &mut TestAppContext,
) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, hosts, client, cx) = mount_workspace(
        cx,
        vec![saved_machine("solo", false)],
        Default::default(),
        1200.,
    );
    let key_solo = HostKey::Remote("solo".into());
    let solo = store_on(&shell, "solo", cx);
    hosts[&key_solo].come_up(&solo, Vec::new(), None, cx);

    // A group member saved in the background does not mix into `solo`.
    let _b = WorkspaceHost::offer(&client, "host-b", cx);
    client.remember_host(saved_machine("host-b", true)).unwrap();
    draw(cx);
    assert_eq!(linked(&shell, cx), std::slice::from_ref(&key_solo));
    assert_eq!(store_of(&shell, cx), solo);

    // Opening that group shows only the group; a new member of the same group
    // then opens immediately beside it while an unrelated standalone host does not.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_group("group".into(), None, window, cx)
        })
    });
    draw(cx);
    assert_eq!(linked(&shell, cx), [HostKey::Remote("host-b".into())]);
    let b = store_on(&shell, "host-b", cx);

    let _c = WorkspaceHost::offer(&client, "host-c", cx);
    client.remember_host(saved_machine("host-c", true)).unwrap();
    let _other = WorkspaceHost::offer(&client, "other-solo", cx);
    client
        .remember_host(saved_machine("other-solo", false))
        .unwrap();
    draw(cx);
    assert_eq!(
        linked(&shell, cx),
        [
            HostKey::Remote("host-b".into()),
            HostKey::Remote("host-c".into()),
        ]
    );
    assert_eq!(store_on(&shell, "host-b", cx), b, "B's link is kept");
}

/// A launch on a host outside the view the device kept makes that host the
/// view: the group left behind does not open beside it when its roster next
/// changes.
#[gpui::test]
fn a_launch_outside_the_kept_view_leaves_that_view_behind(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _hosts, client, cx) = mount_workspace(
        cx,
        vec![saved_machine("solo", false), saved_machine("host-b", true)],
        tcode_client::host::ClientPreferences {
            navigation: Some(serde_json::json!({
                "history": ["threads", "thread"],
                "thread": { "host": "solo", "session_id": "thread-1" },
                "panel": "diff"
            })),
            workspace: Some(tcode_client::host::WorkspacePreference {
                scope: Some(tcode_client::host::WorkspaceScope::Group {
                    group_id: "group".into(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        1200.,
    );
    let key_solo = HostKey::Remote("solo".into());
    assert_eq!(linked(&shell, cx), std::slice::from_ref(&key_solo));

    let _c = WorkspaceHost::offer(&client, "host-c", cx);
    client.remember_host(saved_machine("host-c", true)).unwrap();
    draw(cx);
    assert_eq!(linked(&shell, cx), [key_solo]);
}

/// A desktop launched with no host named is on the view it kept, though
/// that is another machine and it has a host of its own. A kept view whose
/// machine has since been removed has nothing to show: this machine opens
/// in its place and becomes the view.
#[gpui::test]
fn a_launch_that_names_no_host_reopens_the_kept_view(cx: &mut TestAppContext) {
    use tcode_client::host::WorkspaceScope;
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let kept = WorkspaceScope::Host {
        host_id: "studio".into(),
    };
    for (saved, opens, keeps) in [
        (
            vec![saved_machine("studio", false)],
            HostKey::Remote("studio".into()),
            kept.clone(),
        ),
        (Vec::new(), HostKey::Local, WorkspaceScope::Local),
    ] {
        let client = Rc::new(WorkspaceClient {
            saved: RefCell::new(saved.clone()),
            transports: RefCell::default(),
            preferences: RefCell::new(tcode_client::host::ClientPreferences {
                workspace: Some(tcode_client::host::WorkspacePreference {
                    scope: Some(kept.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            last_host_id: RefCell::default(),
            pairs: RefCell::default(),
            watchers: RefCell::default(),
        });
        let _machines: Vec<_> = saved
            .iter()
            .map(|host| WorkspaceHost::offer(&client, &host.host_id, cx))
            .collect();
        let desktop: Rc<dyn ClientHost> = client.clone();
        let (shell, _local, cx) = mount_with(cx, Some(desktop));
        assert_eq!(linked(&shell, cx), [opens]);
        assert_eq!(
            client
                .load_preferences()
                .unwrap()
                .workspace
                .and_then(|workspace| workspace.scope),
            Some(keeps)
        );
    }
}

/// On the Machines page, clicking a machine group opens all of its members
/// together, clicking a member opens that machine alone (reusing its link),
/// and clicking a standalone machine switches to it alone.
#[gpui::test]
fn machines_page_opens_the_clicked_group_member_or_standalone_machine(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, hosts, client, cx) = mount_workspace(
        cx,
        vec![
            saved_machine("solo", false),
            saved_machine("nas", true),
            saved_machine("desk", true),
        ],
        Default::default(),
        393.,
    );
    let key_solo = HostKey::Remote("solo".into());
    let solo = store_on(&shell, "solo", cx);
    hosts[&key_solo].come_up(&solo, Vec::new(), None, cx);
    assert_eq!(linked(&shell, cx), std::slice::from_ref(&key_solo));

    let open_hosts = |cx: &mut VisualTestContext| {
        shell
            .read_with(cx, |shell, _| shell.window_state())
            .update(cx, |state, cx| state.go(Destination::Hosts, cx));
        crate::test_frames::draw_settled(cx);
    };

    // Click the machine group row -> opens only the group's members.
    open_hosts(cx);
    let group_row = cx
        .debug_bounds("host-group-group")
        .expect("machine group entry");
    cx.simulate_click(group_row.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        linked(&shell, cx),
        [
            HostKey::Remote("nas".into()),
            HostKey::Remote("desk".into()),
        ]
    );
    let nas_store = store_on(&shell, "nas", cx);

    // Click a member inside the group -> opens that member alone, keeping its link.
    open_hosts(cx);
    let nas_row = cx.debug_bounds("host-nas").expect("group member row");
    cx.simulate_click(nas_row.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert_eq!(linked(&shell, cx), [HostKey::Remote("nas".into())]);
    assert_eq!(
        store_on(&shell, "nas", cx),
        nas_store,
        "reuses the already open link"
    );

    // Click the standalone machine -> switches to it alone.
    let _solo_again = WorkspaceHost::offer(&client, "solo", cx);
    open_hosts(cx);
    let solo_row = cx.debug_bounds("host-solo").expect("standalone row");
    cx.simulate_click(solo_row.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    assert_eq!(linked(&shell, cx), [key_solo]);
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

/// A thread of a machine that lets this device only view it takes nothing
/// typed into its composer, while a thread of the other machine beside it
/// does.
#[gpui::test]
fn a_view_only_machines_thread_takes_no_message(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, hosts, _client, cx) = mount_workspace(cx, two_hosts(), Default::default(), 1600.);
    cx.update(|_, cx| crate::shortcut::init(cx));
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    hosts[&key_a].come_up_as(&a, Vec::new(), None, true, cx);
    hosts[&key_b].come_up(&b, Vec::new(), None, cx);
    for key in [&key_a, &key_b] {
        send_events(&hosts[key].host, vec![panes_index(&[])]);
    }
    serve_thread(&hosts[&key_a].host, "thread-1");
    a.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx)
    });
    let on_b = b.read_with(cx, |store, _| store.thread_key("thread-1"));
    cx.update(|window, cx| window.dispatch_action(Box::new(OpenInNewPane(on_b)), cx));
    serve_thread(&hosts[&key_b].host, "thread-1");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    assert_eq!(pane_hosts(&shell, cx), [key_a.clone(), key_b.clone()]);

    let panes = shell.read_with(cx, |shell, cx| shell.column.as_ref().unwrap().panes(cx));
    type_into(&panes[0], cx);
    assert_eq!(
        typed_reaches(&hosts[&key_a].host, cx),
        Vec::<String>::new(),
        "the machine that lets this device only view it is sent nothing"
    );
    type_into(&panes[1], cx);
    assert_eq!(typed_reaches(&hosts[&key_b].host, cx), ["thread-1"]);
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
        group_name: None,
        label: None,
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
        Some(None),
        "retrying This machine switches to the local host once it starts"
    );
    let store = shell
        .read_with(cx, |shell, _| shell.local_store())
        .expect("the local host opens");
    crate::store::tests::seed_full_scope(&store, &local_incoming, Vec::new(), cx);
    open_machines(cx);
    crate::test_frames::draw_settled(cx);
    assert!(cx.debug_bounds("hosts-local-failure").is_none());
}

/// While a host moves a project away, its threads take nothing from the
/// composer, and the same thread id on another host of the window still
/// sends.
#[gpui::test]
fn a_moving_projects_thread_cannot_send_and_another_hosts_still_does(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, hosts, cx) = a_thread_of_a_beside_b(cx, 1600.);
    threads_in_a_project(&hosts, cx);
    let b = store_on(&shell, "host-b", cx);
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let on_b = b.read_with(cx, |store, _| store.thread_key("thread-1"));
    cx.update(|window, cx| window.dispatch_action(Box::new(OpenInNewPane(on_b)), cx));
    serve_thread(&hosts[&key_b].host, "thread-1");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    let a = store_on(&shell, "host-a", cx);
    let project_id = a
        .read_with(cx, |store, _| {
            store
                .thread_meta("thread-1")
                .and_then(|meta| meta.project_id.clone())
        })
        .expect("thread-1 is in a project");
    send_events(
        &hosts[&key_a].host,
        vec![(
            Topic::ProjectMoves,
            ServerEvent::ProjectMovesReplaced(vec![tcode_protocol::ProjectMoveStatus {
                move_id: "move-1".into(),
                project_id,
                machine: "host-c".into(),
                phase: tcode_protocol::MovePhase::SendingCode,
                done: 1,
                total: 4,
                commit_unknown: false,
                error: None,
            }]),
        )],
    );
    draw(cx);
    let _ = sent(&hosts[&key_a].host);
    let _ = sent(&hosts[&key_b].host);

    let panes = shell.read_with(cx, |shell, cx| shell.column.as_ref().unwrap().panes(cx));
    for (pane, text) in panes.iter().zip(["to a", "to b"]) {
        type_into(pane, cx);
        cx.simulate_input(text);
        cx.simulate_keystrokes("enter");
        draw(cx);
    }
    assert_eq!(turns_sent(&hosts[&key_a].host), [], "A's project is moving");
    assert_eq!(
        turns_sent(&hosts[&key_b].host),
        [("thread-1".to_owned(), "to b".to_owned())]
    );
}
