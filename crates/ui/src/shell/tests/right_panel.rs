use super::*;

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

/// The right panel comes back to the width the user chose by dragging
/// its edge after the window narrows and widens again, and to its own
/// width once that edge is double clicked.
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

    // A double click on the edge gives the panel its own width back.
    let edge = gpui::point(cx.debug_bounds("right-panel").unwrap().left(), px(400.));
    for click_count in [1, 2] {
        cx.simulate_event(gpui::MouseDownEvent {
            button: MouseButton::Left,
            position: edge,
            click_count,
            ..Default::default()
        });
        cx.simulate_event(gpui::MouseUpEvent {
            button: MouseButton::Left,
            position: edge,
            click_count,
            ..Default::default()
        });
        draw(cx);
    }
    assert_eq!(
        width_at(cx, 1600.),
        panel.size.width,
        "after a double click on its edge"
    );
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
