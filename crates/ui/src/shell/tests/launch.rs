use super::*;

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
            group_name: None,
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

/// The thread a cold start restores stays in the saved checkpoint for as
/// long as its host has said nothing of it, so a launch the host never
/// answers comes back to it the next time. From the first of its content
/// until its baseline settles the checkpoint is without it: a launch that
/// dies loading or first drawing the thread does not open it again.
#[gpui::test]
fn a_restored_thread_leaves_the_checkpoint_only_while_its_content_loads(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, client, cx) = mount_restored(cx, &["threads", "thread"], true);
    let thread = ThreadKey {
        host: HostKey::Remote("last-host".into()),
        session_id: "thread-a".into(),
    };
    let next_launch_opens = |client: &ReturningClient| {
        let saved = client.load_preferences().unwrap().navigation.unwrap();
        NavigationSnapshot::from_preferences(saved).unwrap().thread
    };
    assert_eq!(next_launch_opens(&client).as_ref(), Some(&thread));
    restore_index(&shell, &host, true, cx);
    assert_eq!(
        next_launch_opens(&client).as_ref(),
        Some(&thread),
        "the host lists the thread and has sent nothing of it"
    );

    restore_status(&shell, &host, cx);
    assert_eq!(next_launch_opens(&client), None, "its content is arriving");
    for (topic, event) in [
        (
            Topic::Settings,
            ServerEvent::SettingsSnapshot(tcode_core::settings::Settings::default()),
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
    cx.executor().advance_clock(Duration::from_millis(151));
    draw(cx);
    assert_eq!(next_launch_opens(&client), Some(thread));
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
