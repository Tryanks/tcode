use super::*;

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

/// A press on a subagent's card in its lead's timeline opens the
/// agent's thread beside the lead, in the lead's own pane, with the
/// keyboard; Escape from it closes it and gives the keyboard back to the
/// lead's composer; once shown, the same press leaves the one agent
/// shown rather than opening it twice.
#[gpui::test]
fn a_subagent_card_opens_its_thread_beside_its_lead(cx: &mut TestAppContext) {
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
    let card = cx
        .debug_bounds("thread-card-spawn-1")
        .expect("the subagent's card");
    cx.simulate_click(card.center(), gpui::Modifiers::default());
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
        let card = cx
            .debug_bounds("thread-card-spawn-1")
            .expect("the subagent's card");
        cx.simulate_click(card.center(), gpui::Modifiers::default());
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

/// An agent another agent dispatched, opened beside the lead from that
/// agent, has Back to that agent as well as Close on its card. Back shows
/// the agent it came from beside the lead again, which, the lead's own,
/// has only Close; Close then takes it away.
#[gpui::test]
fn an_agent_of_an_agent_goes_back_to_it_beside_the_lead(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let (topic, ServerEvent::IndexSnapshot(mut index)) = lead_with_agent_index("thread-1") else {
        unreachable!("lead_with_agent_index sends a snapshot");
    };
    let mut nested = index.sessions[2].clone();
    nested.id = "agent-2".into();
    nested.title = "agent-2".into();
    nested.parent_session_id = Some("agent-1".into());
    nested.created_at = 4;
    index.sessions.push(nested);
    send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
    await_restore_update(&shell, cx, |store, _| {
        store.thread_meta("agent-2").is_some()
    });
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Agents, cx)
    });
    crate::test_frames::draw_settled(cx);
    open_agent_from_panel(&shell, &host, cx);
    assert!(cx.debug_bounds("side-back").is_none());

    cx.update(|window, cx| {
        crate::agents_panel::open_agent(
            &store,
            "agent-2".into(),
            crate::shell::side_host_for(&store.read(cx).thread_key("thread-1"), cx),
            None,
            window,
            cx,
        )
    });
    draw(cx);
    serve_thread(&host, "agent-2");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("agent-2"), cx) && store.timeline_settled_for("agent-2")
    });
    crate::test_frames::draw_settled(cx);
    store.read_with(cx, |store, _| {
        assert_eq!(
            store.side_thread_for("thread-1").as_deref(),
            Some("agent-2")
        );
    });
    assert!(cx.debug_bounds("side-close").is_some());

    press_control("side-back", cx);
    serve_thread(&host, "agent-1");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("agent-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    store.read_with(cx, |store, _| {
        assert_eq!(
            store.side_thread_for("thread-1").as_deref(),
            Some("agent-1")
        );
        assert!(!store.is_thread_open("agent-2"));
    });
    assert!(cx.debug_bounds("side-back").is_none());

    press_control("side-close", cx);
    assert!(cx.debug_bounds("chat-side-header").is_none());
    store.read_with(cx, |store, _| {
        assert_eq!(store.side_thread_for("thread-1"), None);
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
