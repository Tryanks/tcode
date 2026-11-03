//! A pane switched to another thread draws it as a new scene: what the
//! thread before left moving, and what the thread shown now did when it was
//! last on screen, carry nothing into the frame that first draws it.

use super::*;

/// `id`'s status and its log, as the host answers the thread's subscriptions.
fn serve_log(
    host: &MountedShell,
    status: tcode_protocol::SessionStatus,
    events: Vec<agent::AgentEvent>,
) {
    let records: Vec<tcode_core::session::StoredEvent> =
        events.into_iter().map(Into::into).collect();
    let session_id = status.session_id.clone();
    send_events(
        host,
        vec![
            (
                Topic::SessionStatus {
                    session_id: session_id.clone(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            ),
            (
                Topic::SessionEvents { session_id },
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
}

fn asked(id: &str) -> agent::AgentEvent {
    agent::AgentEvent::ItemCompleted(agent::ThreadItem {
        id: format!("{id}-ask"),
        parent_item_id: None,
        content: agent::ItemContent::UserMessage {
            text: format!("What is in {id}?"),
            context_len: None,
            attachments: vec![],
        },
    })
}

fn started() -> agent::AgentEvent {
    agent::AgentEvent::TurnStarted {
        turn_id: "turn".into(),
    }
}

fn completed() -> agent::AgentEvent {
    agent::AgentEvent::TurnCompleted {
        turn_id: "turn".into(),
        status: agent::TurnStatus::Completed,
        usage: None,
    }
}

/// `id` with its one turn done.
fn serve_finished(host: &MountedShell, id: &str) {
    let status = session_status(id, std::path::Path::new(PANES_CWD));
    serve_log(host, status, vec![started(), asked(id), completed()]);
}

/// Shows the thread the sidebar lists `key`'s shortcut against, `id`,
/// whose log the host has answered with by the frame that first draws it.
fn switch(shell: &Entity<AppShell>, key: &str, id: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes(key);
    store_of(shell, cx).update(cx, |store, cx| store.drain_host_events_for_test(cx));
    draw(cx);
    assert_eq!(pane_threads(shell, cx), [Some(id.into())]);
}

fn composer(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Entity<crate::composer::Composer> {
    shell.read_with(cx, |shell, cx| {
        shell
            .attachment
            .as_ref()
            .expect("attached")
            .chat(cx)
            .read(cx)
            .composer()
    })
}

/// The composer's motion holds the thread it shows, not every thread it has
/// shown: visiting more threads leaves it as large as visiting one.
#[gpui::test]
fn the_composers_motion_holds_only_the_thread_it_shows(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let threads = 6;
    let sessions = (1..=threads)
        .map(|n| {
            let mut meta = tcode_core::project::SessionMeta::new(
                agent::ProviderKind::Codex,
                PANES_CWD.into(),
                None,
            );
            meta.id = format!("thread-{n}");
            meta.title = meta.id.clone();
            meta.updated_at = (threads - n) as u64;
            meta
        })
        .collect();
    send_events(
        &host,
        vec![(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions,
                projects: Vec::new(),
            }),
        )],
    );
    serve_finished(&host, "thread-1");
    await_restore_update(&shell, cx, |store| {
        store.thread_meta("thread-6").is_some()
            && store.with_active_timeline(|timeline| timeline.turns.len()) == Some(1)
    });
    settle_frames(cx);
    let held = |cx: &VisualTestContext| composer(&shell, cx).read_with(cx, |c, _| c.motion_len());
    let one = held(cx);
    assert!(one > 0, "the composer's controls keep their motion");
    for n in 2..=threads {
        let id = format!("thread-{n}");
        serve_finished(&host, &id);
        switch(&shell, &format!("secondary-{n}"), &id, cx);
        settle_frames(cx);
        assert_eq!(held(cx), one, "after {id}");
    }
}

/// `id` with its one turn done and its terminal drawer open on `terminals`,
/// the last of them active.
fn serve_with_terminals(host: &MountedShell, id: &str, terminals: &[u64]) {
    let mut status = session_status(id, std::path::Path::new(PANES_CWD));
    status.terminals = terminals
        .iter()
        .map(|&id| tcode_protocol::TerminalStatus {
            id,
            title: "shell".into(),
            exited: false,
        })
        .collect();
    status.active_terminal_id = terminals.last().copied();
    status.terminal_open = true;
    serve_log(host, status, vec![started(), asked(id), completed()]);
}

/// The terminal drawer shows another thread's terminals as they are: its
/// tabs are in their places on the first frame, rather than the new
/// thread's folding in and gliding the selection from where the thread
/// before had its own; and so again switching back.
#[gpui::test]
fn the_terminal_drawer_of_a_thread_switched_to_starts_at_rest(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_with_terminals(&host, "thread-1", &[1, 2]);
    await_restore_update(&shell, cx, |store| {
        store.with_active_timeline(|timeline| timeline.turns.len()) == Some(1)
    });
    // Thread-1 was first shown with its drawer shut; the reader opens it.
    store_of(&shell, cx).update(cx, |store, cx| {
        store.toggle_terminal_panel_for("thread-1".into(), cx)
    });
    settle_frames(cx);
    let tabs = |cx: &mut VisualTestContext| {
        ["terminal-tab-1", "terminal-tab-2", "terminal-tab-3"].map(|tab| cx.debug_bounds(tab))
    };
    assert!(tabs(cx)[1].is_some(), "thread-1's terminals");

    for (key, id, terminals) in [
        ("secondary-2", "thread-2", &[3][..]),
        ("secondary-1", "thread-1", &[1, 2][..]),
    ] {
        serve_with_terminals(&host, id, terminals);
        switch(&shell, key, id, cx);
        let first = tabs(cx);
        settle_frames(cx);
        assert_eq!(
            first,
            tabs(cx),
            "{id}'s tabs are at rest from its first frame"
        );
    }
}

/// One frame of a thread appearing: how far its fade has come, and whether
/// its rows and its skeleton were drawn.
#[derive(Debug, Clone, Copy)]
struct Appearing {
    opacity: f32,
    rows: bool,
    skeleton: bool,
}

fn appearing(shell: &Entity<AppShell>, cx: &mut VisualTestContext) -> Appearing {
    let opacity = shell.read_with(cx, |shell, cx| {
        let chat = shell.attachment.as_ref().expect("attached").chat(cx);
        chat.read(cx).thread_opacity
    });
    Appearing {
        opacity,
        rows: cx.debug_bounds("timeline-row-0").is_some(),
        skeleton: cx.debug_bounds("baseline-loading").is_some(),
    }
}

/// The frame drawn now and the `frames` display frames after it.
fn watch(shell: &Entity<AppShell>, frames: usize, cx: &mut VisualTestContext) -> Vec<Appearing> {
    let mut seen = vec![appearing(shell, cx)];
    for _ in 0..frames {
        display_frame(cx);
        seen.push(appearing(shell, cx));
    }
    seen
}

/// The fade a thread appears with starts once its rows are drawn, and plays
/// through to the end: no frame shows it part way in without them.
fn assert_appears_once_drawn(what: &str, seen: &[Appearing]) {
    let started = seen
        .iter()
        .position(|frame| frame.opacity > 0.)
        .unwrap_or_else(|| panic!("{what} never appeared: {seen:?}"));
    assert!(
        seen[started..].iter().all(|frame| frame.rows),
        "{what} faded in without its rows: {seen:?}"
    );
    assert!(
        seen[started..]
            .iter()
            .any(|frame| frame.opacity > 0.05 && frame.opacity < 0.95),
        "{what} appeared at once: {seen:?}"
    );
    assert_eq!(seen.last().map(|frame| frame.opacity), Some(1.), "{what}");
}

/// A thread a pane is switched to, whose log is there by its first frame,
/// appears promptly and still fades in, and only once its rows are drawn;
/// and so again switching back.
#[gpui::test]
fn a_thread_switched_to_fades_in_once_it_is_drawn(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_finished(&host, "thread-1");
    await_restore_update(&shell, cx, |store| {
        store.with_active_timeline(|timeline| timeline.turns.len()) == Some(1)
    });
    settle_frames(cx);
    for (key, id) in [("secondary-2", "thread-2"), ("secondary-1", "thread-1")] {
        serve_finished(&host, id);
        switch(&shell, key, id, cx);
        let seen = watch(&shell, 40, cx);
        assert_appears_once_drawn(id, &seen);
        let started = seen.iter().position(|frame| frame.opacity > 0.).unwrap();
        assert!(started <= 4, "{id} kept the reader waiting: {seen:?}");
        assert!(
            seen.iter().all(|frame| !frame.skeleton),
            "{id} flashed its skeleton"
        );
    }
}

/// A thread whose log the host is slow to send shows nothing new at first,
/// then its skeleton, and fades in from it once its rows are drawn, rather
/// than playing its fade out while it waited.
#[gpui::test]
fn a_thread_slow_to_arrive_fades_in_once_it_has(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_finished(&host, "thread-1");
    await_restore_update(&shell, cx, |store| {
        store.with_active_timeline(|timeline| timeline.turns.len()) == Some(1)
    });
    settle_frames(cx);
    switch(&shell, "secondary-2", "thread-2", cx);
    let waiting = watch(&shell, 40, cx);
    assert!(
        waiting.iter().all(|frame| frame.opacity == 0.),
        "the thread faded in before it arrived: {waiting:?}"
    );
    assert!(!waiting[0].skeleton, "the skeleton shows at once");
    assert!(
        waiting.last().is_some_and(|frame| frame.skeleton),
        "the skeleton shows once the wait is long"
    );

    serve_finished(&host, "thread-2");
    cx.run_until_parked();
    store_of(&shell, cx).update(cx, |store, cx| store.drain_host_events_for_test(cx));
    draw(cx);
    let seen = watch(&shell, 40, cx);
    assert_appears_once_drawn("thread-2", &seen);
    assert!(seen.last().is_some_and(|frame| !frame.skeleton));
}

/// A thread opened from the sidebar into a pane of its own fades in once its
/// rows are drawn.
#[gpui::test]
fn a_thread_opened_into_a_new_pane_fades_in_once_it_is_drawn(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_finished(&host, "thread-1");
    await_restore_update(&shell, cx, |store| {
        store.with_active_timeline(|timeline| timeline.turns.len()) == Some(1)
    });
    settle_frames(cx);
    cx.simulate_keystrokes("secondary-d");
    settle_frames(cx);
    serve_finished(&host, "thread-2");
    cx.simulate_keystrokes("secondary-2");
    store_of(&shell, cx).update(cx, |store, cx| store.drain_host_events_for_test(cx));
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    let seen = watch(&shell, 40, cx);
    assert_appears_once_drawn("thread-2", &seen);
    assert!(
        seen.iter().all(|frame| !frame.skeleton),
        "thread-2 flashed its skeleton: {seen:?}"
    );
}

/// With motion reduced, a thread switched to is there at once when it is
/// ready, and not before.
#[gpui::test]
fn a_thread_switched_to_with_motion_reduced_is_there_once_drawn(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_finished(&host, "thread-1");
    await_restore_update(&shell, cx, |store| {
        store.with_active_timeline(|timeline| timeline.turns.len()) == Some(1)
    });
    settle_frames(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    serve_finished(&host, "thread-2");
    switch(&shell, "secondary-2", "thread-2", cx);
    let seen = watch(&shell, 10, cx);
    let started = seen
        .iter()
        .position(|frame| frame.opacity > 0.)
        .expect("thread-2 appears");
    assert!(
        seen[started..]
            .iter()
            .all(|frame| frame.rows && frame.opacity == 1.)
    );
}
