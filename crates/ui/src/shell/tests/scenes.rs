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
