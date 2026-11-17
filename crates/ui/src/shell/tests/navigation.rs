use super::*;

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
