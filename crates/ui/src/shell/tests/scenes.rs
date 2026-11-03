//! A pane switched to another thread draws it as a new scene: what the
//! thread before left moving, and what the thread shown now did when it was
//! last on screen, carry nothing into the frame that first draws it; and the
//! thread fades in once it can be drawn at rest, not while it loads.

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
            // The agent's question is long, so its bubble is not where its
            // lead's is drawn over it.
            text: if id == "mirror" {
                "What is in the agent's own thread, which goes on asking? ".repeat(6)
            } else {
                format!("What is in {id}?")
            },
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

/// `id` working on its one turn.
fn serve_running(host: &MountedShell, id: &str) {
    let mut status = session_status(id, std::path::Path::new(PANES_CWD));
    status.activity.working = true;
    status.activity.turn_running = true;
    status.running_turn = Some(tcode_core::session::RunningTurn {
        turn: 0,
        started_at: Some(crate::time::now_millis()),
    });
    serve_log(host, status, vec![started(), asked(id)]);
}

/// What the host has sent arrives, and a frame is drawn.
fn arrive(shell: &Entity<AppShell>, cx: &mut VisualTestContext) {
    cx.run_until_parked();
    store_of(shell, cx).update(cx, |store, cx| store.drain_host_events_for_test(cx));
    draw(cx);
}

/// Shows the thread the sidebar lists `key`'s shortcut against, `id`, with
/// what the host has answered by the frame that first draws it.
fn switch(shell: &Entity<AppShell>, key: &str, id: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes(key);
    arrive(shell, cx);
    assert_eq!(pane_threads(shell, cx), [Some(id.into())]);
}

/// A thread left while its composer's controls were on their way (its turn
/// just done: the stop button folding away, the send button coming) and come
/// back to before they would have landed is drawn at rest, rather than
/// resuming where the composer left that thread's controls.
#[gpui::test]
fn the_composer_of_a_thread_come_back_to_starts_at_rest(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_running(&host, "thread-1");
    await_restore_update(&shell, cx, |store| store.composer_state().turn_running);
    settle_frames(cx);
    let send = |cx: &mut VisualTestContext| cx.debug_bounds("send-message");
    assert_eq!(send(cx), None, "the running turn shows its stop button");

    serve_finished(&host, "thread-1");
    await_restore_update(&shell, cx, |store| !store.composer_state().turn_running);
    display_frame(cx);
    serve_finished(&host, "thread-2");
    switch(&shell, "secondary-2", "thread-2", cx);
    display_frame(cx);
    serve_finished(&host, "thread-1");
    switch(&shell, "secondary-1", "thread-1", cx);
    let first = send(cx);
    settle_frames(cx);
    assert!(first.is_some(), "thread-1's send button");
    assert_eq!(first, send(cx), "at rest from its first frame");
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

/// The selector of `id`'s question's bubble.
fn bubble(id: &str) -> &'static str {
    match id {
        "thread-1" => "user-bubble-thread-1-ask",
        "thread-2" => "user-bubble-thread-2-ask",
        "mirror" => "user-bubble-mirror-ask",
        _ => unreachable!("no bubble for {id}"),
    }
}

/// Whether `quad` lies within `bounds`.
fn within(
    quad: &gpui::Bounds<gpui::ScaledPixels>,
    bounds: &gpui::Bounds<gpui::ScaledPixels>,
) -> bool {
    quad.origin.x >= bounds.origin.x
        && quad.origin.y >= bounds.origin.y
        && quad.origin.x + quad.size.width <= bounds.origin.x + bounds.size.width
        && quad.origin.y + quad.size.height <= bounds.origin.y + bounds.size.height
}

/// How opaque `id`'s question's bubble is painted, by its fill's alpha;
/// nothing while it is not drawn.
fn bubble_fill(id: &str, cx: &mut VisualTestContext) -> Option<f32> {
    let bounds = cx.debug_bounds(bubble(id))?;
    cx.update(|window, _| {
        let bounds = bounds.scale(window.scale_factor());
        Some(
            window
                .painted_quads()
                .iter()
                .filter(|quad| within(&quad.bounds, &bounds))
                .filter_map(|quad| quad.background.as_solid())
                .map(|colour| colour.a)
                .fold(0., f32::max),
        )
    })
}

/// One frame of a thread appearing: how far in its question's bubble is
/// painted against how it is at rest (none while it is not drawn), and
/// whether a skeleton and a pane's waiting hint were drawn.
#[derive(Debug, Clone, Copy)]
struct Appearing {
    opacity: Option<f32>,
    skeleton: bool,
    waiting: bool,
}

impl Appearing {
    fn shown(&self) -> f32 {
        self.opacity.unwrap_or_default()
    }
}

fn appearing(id: &str, rest: f32, cx: &mut VisualTestContext) -> Appearing {
    Appearing {
        opacity: bubble_fill(id, cx).map(|fill| fill / rest),
        skeleton: cx.debug_bounds("baseline-loading").is_some(),
        waiting: cx.debug_bounds("waiting-pane").is_some(),
    }
}

/// The frame drawn now and the `frames` display frames after it.
fn watch(id: &str, rest: f32, frames: usize, cx: &mut VisualTestContext) -> Vec<Appearing> {
    let mut seen = vec![appearing(id, rest, cx)];
    for _ in 0..frames {
        display_frame(cx);
        seen.push(appearing(id, rest, cx));
    }
    seen
}

/// The fade a thread appears with starts once its rows are drawn, and plays
/// through to the end: no frame paints it part way in before.
fn assert_appears_once_drawn(what: &str, seen: &[Appearing]) {
    let started = seen
        .iter()
        .position(|frame| frame.shown() > 0.01)
        .unwrap_or_else(|| panic!("{what} never appeared: {seen:?}"));
    assert!(
        seen[started..].iter().all(|frame| frame.opacity.is_some()),
        "{what} faded in without its rows: {seen:?}"
    );
    assert!(
        seen[started..]
            .iter()
            .any(|frame| frame.shown() > 0.05 && frame.shown() < 0.95),
        "{what} appeared at once: {seen:?}"
    );
    assert!(
        seen.last()
            .is_some_and(|frame| (frame.shown() - 1.).abs() < 0.01),
        "{what} came all the way in: {seen:?}"
    );
}

/// thread-1 open, its one turn done, and how opaque its question's bubble
/// is painted at rest.
fn mount_finished(
    cx: &mut TestAppContext,
) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext, f32) {
    let (shell, host, cx) = mount_panes(cx);
    serve_finished(&host, "thread-1");
    await_restore_update(&shell, cx, |store| {
        store.with_active_timeline(|timeline| timeline.turns.len()) == Some(1)
    });
    settle_frames(cx);
    let rest = bubble_fill("thread-1", cx).expect("thread-1's question");
    assert!(rest > 0., "the bubble has a fill");
    (shell, host, cx, rest)
}

/// A thread a pane is switched to, whose log is there by its first frame,
/// appears promptly and still fades in, and only once its rows are drawn;
/// and so again switching back.
#[gpui::test]
fn a_thread_switched_to_fades_in_once_it_is_drawn(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx, rest) = mount_finished(cx);
    for (key, id) in [("secondary-2", "thread-2"), ("secondary-1", "thread-1")] {
        serve_finished(&host, id);
        switch(&shell, key, id, cx);
        let seen = watch(id, rest, 40, cx);
        assert_appears_once_drawn(id, &seen);
        let started = seen.iter().position(|frame| frame.shown() > 0.01).unwrap();
        assert!(started <= 4, "{id} kept the reader waiting: {seen:?}");
        assert!(
            seen.iter().all(|frame| !frame.skeleton),
            "{id} flashed its skeleton: {seen:?}"
        );
    }
}

/// A thread whose log the host is slow to send shows nothing new at first,
/// then its skeleton, and fades in from it once its rows are drawn, rather
/// than playing its fade out while it waited.
#[gpui::test]
fn a_thread_slow_to_arrive_fades_in_once_it_has(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx, rest) = mount_finished(cx);
    switch(&shell, "secondary-2", "thread-2", cx);
    let waiting = watch("thread-2", rest, 40, cx);
    assert!(
        waiting.iter().all(|frame| frame.shown() == 0.),
        "the thread faded in before it arrived: {waiting:?}"
    );
    assert!(!waiting[0].skeleton, "the skeleton shows at once");
    assert!(
        waiting.last().is_some_and(|frame| frame.skeleton),
        "the skeleton shows once the wait is long"
    );

    serve_finished(&host, "thread-2");
    arrive(&shell, cx);
    let seen = watch("thread-2", rest, 40, cx);
    assert_appears_once_drawn("thread-2", &seen);
    assert!(seen.last().is_some_and(|frame| !frame.skeleton));
}

/// A thread opened into a pane that waited beside another keeps the waiting
/// hint until it can be drawn, however long its log takes short of the
/// skeleton's patience, then cross-fades straight from the hint: no frame
/// shows neither.
#[gpui::test]
fn a_waiting_pane_keeps_its_hint_until_its_thread_is_drawn(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx, rest) = mount_finished(cx);
    cx.simulate_keystrokes("secondary-d");
    settle_frames(cx);
    assert!(cx.debug_bounds("waiting-pane").is_some());
    cx.simulate_keystrokes("secondary-2");
    arrive(&shell, cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    // The log comes past the hint's own fade, short of the skeleton's wait.
    let mut seen = watch("thread-2", rest, 15, cx);
    serve_finished(&host, "thread-2");
    arrive(&shell, cx);
    seen.extend(watch("thread-2", rest, 40, cx));
    assert!(
        seen.iter().all(|frame| frame.waiting || frame.shown() > 0.),
        "the pane went blank before its thread: {seen:?}"
    );
    assert!(
        seen.iter().all(|frame| !frame.skeleton),
        "the pane flashed a skeleton: {seen:?}"
    );
    assert_appears_once_drawn("thread-2", &seen);
}

/// With motion reduced, a thread switched to is there at once when it is
/// ready, and not before.
#[gpui::test]
fn a_thread_switched_to_with_motion_reduced_is_there_once_drawn(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx, rest) = mount_finished(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    serve_finished(&host, "thread-2");
    switch(&shell, "secondary-2", "thread-2", cx);
    let seen = watch("thread-2", rest, 10, cx);
    let started = seen
        .iter()
        .position(|frame| frame.shown() > 0.)
        .expect("thread-2 appears");
    assert!(
        seen[started..]
            .iter()
            .all(|frame| (frame.shown() - 1.).abs() < 0.01),
        "{seen:?}"
    );
}

/// How many surfaces are painted over the compact page's body, whose
/// bounds are `body`, and how far in thread-2's question is.
fn compact_body(
    body: gpui::Bounds<gpui::ScaledPixels>,
    rest: f32,
    cx: &mut VisualTestContext,
) -> (usize, f32) {
    let surfaces = cx.update(|window, cx| {
        let surface = crate::material::content_surface(cx);
        window
            .painted_quads()
            .iter()
            .filter(|quad| quad.bounds == body)
            .filter_map(|quad| quad.background.as_solid())
            .filter(|colour| colour.a > 0. && colour.h == surface.h && colour.l == surface.l)
            .count()
    });
    let shown = bubble_fill("thread-2", cx).map_or(0., |fill| fill / rest);
    (surfaces, shown)
}

/// Back from an agent's thread to its lead on the compact page: the lead's
/// chat rises over the agent's once its rows are drawn, rather than rising
/// at once, covering the agent's thread with an empty surface while the
/// lead, not opened before, loads.
#[gpui::test]
fn the_compact_page_raises_a_lead_once_it_is_drawn(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx, _) = mount_finished(cx);
    let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
        unreachable!("panes_index sends a snapshot");
    };
    let mut mirror = index.sessions[1].clone();
    mirror.id = "mirror".into();
    mirror.title = "mirror".into();
    mirror.parent_session_id = Some("thread-2".into());
    index.sessions.push(mirror);
    send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
    as_mobile(cx);
    cx.simulate_resize(size(px(393.), px(852.)));
    serve_finished(&host, "mirror");
    store_of(&shell, cx).update(cx, |store, cx| store.select_session("mirror".into(), cx));
    await_restore_update(&shell, cx, |store| {
        store.with_active_timeline(|timeline| timeline.turns.len()) == Some(1)
            && store.thread_meta("mirror").is_some()
    });
    settle_frames(cx);
    assert!(cx.debug_bounds("compact-thread-page").is_some());
    let rest = bubble_fill("mirror", cx).expect("the agent's question");
    // The body: the agent's chat's surface, the nearest beneath its question.
    let question = cx.debug_bounds(bubble("mirror")).expect("drawn");
    let body = cx.update(|window, cx| {
        let surface = crate::material::content_surface(cx);
        let question = question.scale(window.scale_factor());
        window
            .painted_quads()
            .iter()
            .filter(|quad| within(&question, &quad.bounds))
            .filter(|quad| {
                quad.background
                    .as_solid()
                    .is_some_and(|colour| colour.h == surface.h && colour.l == surface.l)
            })
            .map(|quad| quad.bounds)
            .min_by(|a, b| {
                (a.size.width.0 * a.size.height.0).total_cmp(&(b.size.width.0 * b.size.height.0))
            })
            .expect("the chat's surface")
    });
    assert!(rest > 0.);

    let (beneath, _) = compact_body(body, rest, cx);
    shell.update_in(cx, |shell, window, cx| assert!(shell.back(window, cx)));
    draw(cx);
    let mut seen = Vec::new();
    for frame in 0..55 {
        if frame == 15 {
            serve_finished(&host, "thread-2");
            arrive(&shell, cx);
        }
        display_frame(cx);
        seen.push(compact_body(body, rest, cx));
    }
    assert!(
        seen.iter()
            .all(|(surfaces, shown)| *surfaces <= beneath || *shown > 0.),
        "the lead rose before it was drawn: {seen:?}"
    );
    assert!(
        seen.iter().any(|(_, shown)| *shown > 0.05 && *shown < 0.95),
        "the lead rose at once: {seen:?}"
    );
    assert!(
        seen.last()
            .is_some_and(|(_, shown)| (*shown - 1.).abs() < 0.01),
        "{seen:?}"
    );
}
