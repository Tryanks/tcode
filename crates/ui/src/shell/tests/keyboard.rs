use super::*;

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
    cx.simulate_resize(size(px(1200.), px(640.)));
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

/// A question taller than what a software keyboard leaves of a phone's
/// thread page: the answer field being typed in is brought into view above
/// the keyboard, in the panels over the card, and the card stays on screen
/// under it. An answer of several lines is shown whole, and stays in view
/// as it grows. The turn runs, so motion is reduced and the window rests.
#[gpui::test]
fn a_question_answer_field_stays_above_the_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (_shell, host, cx) = mount_panes(cx);
    as_mobile(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    cx.simulate_resize(size(px(393.), px(852.)));
    let mut status = session_status("thread-1", std::path::Path::new(PANES_CWD));
    status.activity.working = true;
    status.activity.turn_running = true;
    status.running_turn = Some(tcode_core::session::RunningTurn {
        turn: 0,
        started_at: None,
    });
    status.pending_user_input = Some(tcode_core::session::PendingUserInput {
        request_id: "ask".into(),
        parent_item_id: None,
        questions: vec![agent::UserInputQuestion {
            id: "storage".into(),
            header: "Storage".into(),
            question: "Which storage should the migration use?".into(),
            options: (0..4)
                .map(|n| agent::UserInputOption {
                    label: format!("Option {n}"),
                    description: "What choosing this one would mean for the migration.".into(),
                })
                .collect(),
            multi_select: false,
            prefill: None,
            optional: false,
        }],
        delivery: agent::UserInputDelivery::Blocking,
    });
    scenes::serve_log(&host, status, vec![scenes::started()]);
    crate::test_frames::draw_settled(cx);
    let bounds = |selector: &'static str, cx: &mut VisualTestContext| {
        cx.debug_bounds(selector).expect(selector)
    };
    let field = bounds("ui-custom-answer", cx);
    cx.simulate_click(field.center(), gpui::Modifiers::default());
    draw(cx);
    // The status bar and the keyboard cover the window.
    crate::test_frames::occlude(
        cx,
        gpui::Edges {
            top: px(59.),
            bottom: px(336.),
            ..Default::default()
        },
    );
    crate::test_frames::draw_settled(cx);
    let shown = cx.update(|window, _| window.fully_visible_bounds());
    let panels = bounds("composer-panels", cx);
    let field = bounds("ui-custom-answer", cx);
    let card = bounds("composer-card", cx);
    assert!(
        panels.top() >= shown.top() && card.bottom() <= shown.bottom(),
        "the composer is drawn in what the keyboard leaves: {panels:?} and {card:?} in {shown:?}"
    );
    assert!(
        field.top() >= panels.top() && field.bottom() <= panels.bottom(),
        "the answer field is in view in its panels: {field:?} in {panels:?}"
    );
    cx.simulate_input("one\ntwo\nthree");
    crate::test_frames::draw_settled(cx);
    let grown = bounds("ui-custom-answer", cx);
    let panels = bounds("composer-panels", cx);
    assert!(
        grown.size.height > field.size.height,
        "the field shows every line of the answer: {grown:?} after {field:?}"
    );
    assert!(
        grown.top() >= panels.top() && grown.bottom() <= panels.bottom(),
        "the grown field is still in view in its panels: {grown:?} in {panels:?}"
    );
}
