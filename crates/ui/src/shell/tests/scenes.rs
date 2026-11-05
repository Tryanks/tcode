//! A pane switched to another thread draws it as a new scene: what the
//! thread before left moving, and what the thread shown now did when it was
//! last on screen, carry nothing into the frame that first draws it; and the
//! thread fades in once it can be drawn at rest, not while it loads.

use super::*;

/// `id`'s status and its log, as the host answers the thread's subscriptions.
pub(super) fn serve_log(
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

pub(super) fn asked(id: &str) -> agent::AgentEvent {
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

pub(super) fn started() -> agent::AgentEvent {
    agent::AgentEvent::TurnStarted {
        turn_id: "turn".into(),
    }
}

pub(super) fn completed() -> agent::AgentEvent {
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
    await_restore_update(&shell, cx, |store, cx| {
        store.composer_state_for(Some("thread-1"), cx).turn_running
    });
    settle_frames(cx);
    let send = |cx: &mut VisualTestContext| cx.debug_bounds("send-message");
    assert_eq!(send(cx), None, "the running turn shows its stop button");

    serve_finished(&host, "thread-1");
    await_restore_update(&shell, cx, |store, cx| {
        !store.composer_state_for(Some("thread-1"), cx).turn_running
    });
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
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx) == Some(1)
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

/// The right panel shows a thread switched to as that thread left it: open
/// or shut from the first frame, docked beside a wide chat or covering a
/// narrow one, the chat already at its width beside it, rather than sliding shut as if the reader had closed it on leaving a thread that
/// had it open, or open on coming back; and so again switching back.
#[gpui::test]
fn the_right_panel_of_a_thread_switched_to_starts_at_rest(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_finished(&host, "thread-1");
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx) == Some(1)
    });
    store_of(&shell, cx).update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
    });
    settle_frames(cx);
    let panel = |cx: &mut VisualTestContext| {
        cx.debug_bounds("right-panel")
            .zip(cx.debug_bounds("pane-conversation"))
    };

    for width in [2000., 900.] {
        resize(cx, width);
        settle_frames(cx);
        let open = panel(cx);
        assert!(open.is_some(), "thread-1's panel at {width}px");
        for (key, id, shown) in [
            ("secondary-2", "thread-2", None),
            ("secondary-1", "thread-1", open),
        ] {
            serve_finished(&host, id);
            switch(&shell, key, id, cx);
            assert_eq!(
                panel(cx),
                shown,
                "{id}'s panel on its first frame at {width}px"
            );
            settle_frames(cx);
            assert_eq!(panel(cx), shown, "{id}'s panel at rest at {width}px");
        }
    }

    // Within one thread, the reader closing the panel still slides it out.
    resize(cx, 2000.);
    settle_frames(cx);
    let open = cx.debug_bounds("right-panel").expect("thread-1's panel");
    assert_eq!(
        cx.debug_bounds("pane-conversation")
            .map(|chat| chat.right()),
        Some(open.left()),
        "docked at 2000px"
    );
    store_of(&shell, cx).update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
    });
    draw(cx);
    display_frame(cx);
    display_frame(cx);
    let closing = cx
        .debug_bounds("right-panel")
        .expect("the panel on its way");
    assert!(
        closing.left() > open.left() && closing.left() < open.right(),
        "the panel slides out: {open:?} -> {closing:?}"
    );
    settle_frames(cx);
    assert_eq!(cx.debug_bounds("right-panel"), None, "the panel shut");
}

/// The right panel shows a thread switched to on the tab that thread left
/// it on, from the first frame: the tab the thread before showed does not
/// fade out over it; and so again switching back.
#[gpui::test]
fn the_right_panel_of_a_thread_switched_to_shows_its_tab_at_once(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_finished(&host, "thread-1");
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx) == Some(1)
    });
    let store = store_of(&shell, cx);
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
    });
    settle_frames(cx);
    serve_finished(&host, "thread-2");
    switch(&shell, "secondary-2", "thread-2", cx);
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-2", tcode_core::ui::RightTab::PullRequests, cx)
    });
    settle_frames(cx);
    let diff = |cx: &mut VisualTestContext| cx.debug_bounds("diff-tab-body").is_some();
    assert!(!diff(cx), "thread-2 shows its pull requests");

    for (key, id, on_diff) in [
        ("secondary-1", "thread-1", true),
        ("secondary-2", "thread-2", false),
    ] {
        serve_finished(&host, id);
        switch(&shell, key, id, cx);
        assert!(
            cx.debug_bounds("right-panel").is_some(),
            "{id}'s panel is open"
        );
        assert_eq!(diff(cx), on_diff, "{id}'s tab on its first frame");
        settle_frames(cx);
        assert_eq!(diff(cx), on_diff, "{id}'s tab at rest");
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
    fill_of(bubble(id), cx)
}

/// How far in what `selector` names is drawn: the fullest colour painted
/// within its bounds.
pub(super) fn fill_of(selector: &'static str, cx: &mut VisualTestContext) -> Option<f32> {
    let bounds = cx.debug_bounds(selector)?;
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
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx) == Some(1)
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

/// What thread-1 holds as the pane leaves it and comes back to it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Holding {
    /// One turn done.
    Plain,
    /// A turn under way, its question asked, one steer taken into the turn
    /// and one waiting as its chip.
    Steers,
    /// One turn done, its right panel open on the diff.
    RightPanel,
    /// One turn done, its terminal drawer open.
    Drawer,
    /// One turn done, its right panel open on its agents.
    AgentsPanel,
    /// A turn under way running an agent, whose window it shows.
    Subagent,
    /// A turn under way, its answer streaming in.
    Streaming,
    /// A turn under way, its answer streaming in and still revealing as the
    /// pane leaves; while away the turn finishes the answer and goes on
    /// past it, which the host sends once the pane is back.
    StreamedOn,
    /// One turn done, which ran a command, with a goal and a plan: the
    /// side rail shows their cards.
    Rail,
}

/// What [`Holding::StreamedOn`]'s answer has streamed by the time the pane
/// leaves: the first part, and the part it is revealing as it leaves.
fn streamed_on(part: usize) -> String {
    format!("Part {part} reads `the build` and `the tests` that cover them. ").repeat(6)
}

/// `id`'s status and log as it holds `holding`.
fn serve_holding(host: &MountedShell, id: &str, holding: Holding) {
    let mut status = session_status(id, std::path::Path::new(PANES_CWD));
    let mut events = vec![started(), asked(id)];
    let running = matches!(
        holding,
        Holding::Steers | Holding::Subagent | Holding::Streaming | Holding::StreamedOn
    );
    if running {
        status.activity.working = true;
        status.activity.turn_running = true;
        status.running_turn = Some(tcode_core::session::RunningTurn {
            turn: 0,
            started_at: Some(crate::time::now_millis()),
        });
    }
    match holding {
        Holding::Steers => events.extend([
            agent::AgentEvent::SteerRequested {
                request_id: "taken".into(),
                text: "Also check the docs.".into(),
                attachments: Vec::new(),
            },
            agent::AgentEvent::SteerAccepted {
                request_id: "taken".into(),
            },
            agent::AgentEvent::SteerRequested {
                request_id: "waiting".into(),
                text: "And the tests.".into(),
                attachments: Vec::new(),
            },
        ]),
        Holding::Drawer => {
            status.terminals = vec![tcode_protocol::TerminalStatus {
                id: 1,
                title: "shell".into(),
                exited: false,
            }];
            status.active_terminal_id = Some(1);
            status.terminal_open = true;
        }
        Holding::Subagent => events.push(agent::AgentEvent::ItemStarted(agent::ThreadItem {
            id: format!("{id}-agent"),
            parent_item_id: None,
            content: agent::ItemContent::Subagent {
                agent_type: "Explore".into(),
                description: "Survey the code".into(),
                status: agent::ItemStatus::InProgress,
                summary: None,
                model: None,
                effort: None,
            },
        })),
        Holding::Streaming => events.push(agent::AgentEvent::Delta {
            item_id: format!("{id}-answer"),
            kind: agent::DeltaKind::AssistantText,
            text: "Reading the build files, then the tests that cover them. ".repeat(4),
        }),
        Holding::StreamedOn => events.push(answer_delta(id, streamed_on(0))),
        Holding::Rail => {
            status.goal = Some(agent::Goal {
                objective: format!("Keep {id} going"),
                status: agent::GoalStatus::Active,
                tokens_used: Some(12_000),
                token_budget: Some(200_000),
                time_used_secs: Some(90),
                checks: Some(2),
                last_check: None,
            });
            send_events(
                host,
                vec![(
                    Topic::SessionPlan {
                        session_id: id.into(),
                    },
                    ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                        session_id: id.into(),
                        steps: (0..3)
                            .map(|step| agent::PlanStep {
                                step: format!("Step {step} of {id}'s plan"),
                                status: agent::PlanStepStatus::Pending,
                            })
                            .collect(),
                        explanation: None,
                    }),
                )],
            );
            events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: format!("{id}-command"),
                parent_item_id: None,
                content: agent::ItemContent::CommandExecution {
                    command: "cargo test".into(),
                    output: "ok\n".into(),
                    exit_code: Some(0),
                    status: agent::ItemStatus::Completed,
                },
            }));
        }
        Holding::Plain | Holding::RightPanel | Holding::AgentsPanel => {}
    }
    if !running {
        events.push(completed());
    }
    serve_log(host, status, events);
}

fn answer_delta(id: &str, text: String) -> agent::AgentEvent {
    agent::AgentEvent::Delta {
        item_id: format!("{id}-answer"),
        kind: agent::DeltaKind::AssistantText,
        text,
    }
}

/// `id`'s log once [`Holding::StreamedOn`]'s turn went on while the pane
/// was away: the answer it left streaming finished, then a thought, a
/// command and another answer, the turn still under way.
fn serve_streamed_on(host: &MountedShell, id: &str) {
    let mut status = session_status(id, std::path::Path::new(PANES_CWD));
    status.activity.working = true;
    status.activity.turn_running = true;
    status.running_turn = Some(tcode_core::session::RunningTurn {
        turn: 0,
        started_at: Some(crate::time::now_millis()),
    });
    let item = |id: String, content| {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id,
            parent_item_id: None,
            content,
        })
    };
    let answer = (0..3).map(streamed_on).collect::<String>();
    let events = vec![
        started(),
        asked(id),
        answer_delta(id, streamed_on(0)),
        answer_delta(id, streamed_on(1)),
        answer_delta(id, streamed_on(2)),
        item(
            format!("{id}-answer"),
            agent::ItemContent::AssistantMessage {
                text: answer,
                summarized: false,
            },
        ),
        item(
            format!("{id}-thought"),
            agent::ItemContent::Reasoning {
                text: "Which of the tests cover the build?".into(),
            },
        ),
        item(
            format!("{id}-command"),
            agent::ItemContent::CommandExecution {
                command: "cargo test".into(),
                output: "ok\n".into(),
                exit_code: Some(0),
                status: agent::ItemStatus::Completed,
            },
        ),
        item(
            format!("{id}-next"),
            agent::ItemContent::AssistantMessage {
                text: "The tests pass, `all of them`.".into(),
                summarized: false,
            },
        ),
    ];
    serve_log(host, status, events);
}

/// thread-2 as the pane switches to it: one turn done, a long question
/// and a long answer, so nothing of it lies where thread-1's lies.
fn serve_other(host: &MountedShell) {
    let status = session_status("thread-2", std::path::Path::new(PANES_CWD));
    let events = vec![
        started(),
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: "thread-2-ask".into(),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: "What is in thread-2, which asks at length? ".repeat(8),
                context_len: None,
                attachments: vec![],
            },
        }),
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: "thread-2-answer".into(),
            parent_item_id: None,
            content: agent::ItemContent::AssistantMessage {
                text: "A few things, each of them at some length. ".repeat(12),
                summarized: false,
            },
        }),
        completed(),
    ];
    serve_log(host, status, events);
}

/// Every quad the window painted right of the sidebar, by where it lies:
/// what moves while its colour holds, or fades while it sits still, is told
/// apart from what is at rest. A text caret, which blinks, is left out; so
/// is the sidebar, whose selection follows the reader's own choice.
fn painted(cx: &mut VisualTestContext) -> Vec<[i32; 4]> {
    cx.update(|window, _| {
        let column = SIDEBAR_WIDTH * window.scale_factor();
        let mut quads: Vec<[i32; 4]> = crate::test_frames::painted_quads(window)
            .iter()
            .filter(|quad| {
                quad.background
                    .as_solid()
                    .is_some_and(|colour| colour.a > 0.)
                    && quad.bounds.size.width.0 > 3.
                    && quad.bounds.origin.x.0 >= column
            })
            .map(|quad| {
                let bounds = quad.bounds;
                [
                    bounds.origin.x.0,
                    bounds.origin.y.0,
                    bounds.size.width.0,
                    bounds.size.height.0,
                ]
                .map(|value| value.round() as i32)
            })
            .collect();
        quads.sort_unstable();
        quads
    })
}

/// What one painting has that the other has not.
fn differences(a: &[[i32; 4]], b: &[[i32; 4]]) -> (Vec<[i32; 4]>, Vec<[i32; 4]>) {
    let only = |a: &[[i32; 4]], b: &[[i32; 4]]| {
        let mut b = b.to_vec();
        a.iter()
            .filter(|quad| match b.iter().position(|other| other == *quad) {
                Some(at) => {
                    b.remove(at);
                    false
                }
                None => true,
            })
            .copied()
            .collect::<Vec<_>>()
    };
    (only(a, b), only(b, a))
}

/// A pane switched to another thread, and back, draws each as a new scene:
/// on the first frame that shows a thread, everything painted is where and
/// as large as it rests, whatever the thread left was showing (its steers,
/// its right panel, its terminal drawer, its agents, an agent's window, an
/// answer streaming in, its side rail's cards) and whatever the thread
/// shown had when it was last on screen, an answer it left still revealing
/// included, which the thread finished while away. Only the thread's fade
/// in, which moves nothing, plays.
#[gpui::test]
fn a_thread_switched_to_starts_at_rest_whatever_either_holds(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    let mut failures = Vec::new();
    for holding in [
        Holding::Plain,
        Holding::Steers,
        Holding::RightPanel,
        Holding::Drawer,
        Holding::AgentsPanel,
        Holding::Subagent,
        Holding::Streaming,
        Holding::StreamedOn,
        Holding::Rail,
    ] {
        serve_holding(&host, "thread-1", holding);
        switch(&shell, "secondary-1", "thread-1", cx);
        settle_frames(cx);
        let opened = store.update(cx, |store, cx| {
            match holding {
                Holding::RightPanel => {
                    store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
                }
                Holding::AgentsPanel => {
                    store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Agents, cx)
                }
                Holding::Drawer => store.toggle_terminal_panel_for("thread-1".into(), cx),
                _ => return false,
            }
            true
        });
        if opened {
            settle_frames(cx);
        }
        if holding == Holding::StreamedOn {
            // More of the answer streams in, and the pane leaves on the
            // frame after.
            send_events(
                &host,
                vec![(
                    Topic::SessionEvents {
                        session_id: "thread-1".into(),
                    },
                    ServerEvent::SessionEvent(answer_delta("thread-1", streamed_on(1)).into()),
                )],
            );
            arrive(&shell, cx);
            display_frame(cx);
        }
        for (key, id) in [("secondary-2", "thread-2"), ("secondary-1", "thread-1")] {
            match (id, holding) {
                // The pane draws what the thread held as it left until the
                // host has sent what it did since.
                ("thread-1", Holding::StreamedOn) => {
                    switch(&shell, key, id, cx);
                    serve_streamed_on(&host, id);
                    arrive(&shell, cx);
                }
                ("thread-1", _) => {
                    serve_holding(&host, id, holding);
                    switch(&shell, key, id, cx);
                }
                _ => {
                    serve_other(&host);
                    switch(&shell, key, id, cx);
                }
            }
            // The first frame that shows the thread, however little.
            for _ in 0..10 {
                if bubble_fill(id, cx).is_some_and(|fill| fill > 0.) {
                    break;
                }
                display_frame(cx);
            }
            let first = (painted(cx), cx.debug_bounds("right-panel"));
            settle_frames(cx);
            let rest = (painted(cx), cx.debug_bounds("right-panel"));
            if first != rest {
                let (moving, resting) = differences(&first.0, &rest.0);
                failures.push(format!(
                    "{holding:?}, switched to {id}: panel {:?} -> {:?}; \
                     first frame only {moving:?}; at rest only {resting:?}",
                    first.1, rest.1
                ));
            }
        }
        // thread-1 leaves its panel and drawer shut for the next case.
        store.update(cx, |store, cx| match holding {
            Holding::RightPanel | Holding::AgentsPanel => {
                store.close_right_panel_for(Some("thread-1"), cx)
            }
            Holding::Drawer => store.toggle_terminal_panel_for("thread-1".into(), cx),
            _ => {}
        });
        settle_frames(cx);
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// How light the last frame is at `probe`, by relative luminance, over a
/// black or a white `backdrop` (what the window's translucent canvas lets
/// through): every solid fill and every shadow drawn there, composited in
/// the order they were drawn, a shadow as strong there as its blur makes it
/// (GPUI's own Gaussian, its corners taken as square).
fn luminance_at(
    window: &gpui::Window,
    probe: gpui::Point<gpui::ScaledPixels>,
    backdrop: f32,
) -> f32 {
    let mut drawn: Vec<(u32, gpui::Hsla)> = crate::test_frames::painted_quads(window)
        .iter()
        .filter(|quad| quad.bounds.contains(&probe) && quad.content_mask.bounds.contains(&probe))
        .filter_map(|quad| Some((quad.order, quad.background.as_solid()?)))
        .collect();
    // Abramowitz and Stegun's 7.1.26.
    let erf = |x: f32| {
        let t = 1. / (1. + 0.327_591_1 * x.abs());
        let poly = t
            * (0.254_829_6
                + t * (-0.284_496_7 + t * (1.421_413_8 + t * (-1.453_152 + t * 1.061_405_4))));
        (1. - poly * (-x * x).exp()).copysign(x)
    };
    let across = |at: f32, from: f32, to: f32, sigma: f32| {
        if sigma <= 0. {
            return if (from..to).contains(&at) { 1. } else { 0. };
        }
        let scale = std::f32::consts::FRAC_1_SQRT_2 / sigma;
        0.5 * (erf((to - at) * scale) - erf((from - at) * scale))
    };
    drawn.extend(
        crate::test_frames::painted_shadows(window)
            .iter()
            .filter(|shadow| shadow.content_mask.bounds.contains(&probe))
            .map(|shadow| {
                let (bounds, sigma) = (shadow.bounds, shadow.blur_radius.0);
                let strength = across(probe.x.0, bounds.left().0, bounds.right().0, sigma)
                    * across(probe.y.0, bounds.top().0, bounds.bottom().0, sigma);
                let mut colour = shadow.color;
                colour.a *= strength;
                (shadow.order, colour)
            }),
    );
    drawn.sort_by_key(|(order, _)| *order);
    let [r, g, b] = drawn.iter().fold([backdrop; 3], |under, (_, colour)| {
        let colour = gpui::Rgba::from(*colour);
        let over = |channel: f32, under: f32| channel * colour.a + under * (1. - colour.a);
        [
            over(colour.r, under[0]),
            over(colour.g, under[1]),
            over(colour.b, under[2]),
        ]
    });
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// The cards [`Holding::Rail`] shows on the side rail.
const RAIL_CARDS: [&str; 3] = ["goal", "plan", "commands"];

/// How light each of the rail's cards is drawn at its centre in the last
/// frame, over a black and a white backdrop; nothing for a card not drawn.
fn rail_lightness(cx: &mut VisualTestContext) -> [Option<[f32; 2]>; 3] {
    let cards = RAIL_CARDS.map(|card| cx.debug_bounds(format!("rail-card-frame-{card}").leak()));
    cx.update(|window, _| {
        let scale = window.scale_factor();
        cards.map(|card| {
            let centre = card?.center().scale(scale);
            Some([0., 1.].map(|backdrop| luminance_at(window, centre, backdrop)))
        })
    })
}

/// The rail's cards of a thread switched to fade in with the thread from
/// the canvas to the card through nothing darker than either, in either
/// theme: a card's shadow does not show through its fill while the fill is
/// faint.
#[gpui::test]
fn the_rail_cards_of_a_thread_switched_to_fade_in_through_nothing_darker(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_holding(&host, "thread-1", Holding::Rail);
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx) == Some(1)
    });
    settle_frames(cx);
    let mut failures = Vec::new();
    for mode in [
        crate::theme::ThemeMode::Light,
        crate::theme::ThemeMode::Dark,
    ] {
        cx.update(|window, cx| crate::theme::change_mode(mode, Some(window), cx));
        settle_frames(cx);
        for (key, id) in [("secondary-2", "thread-2"), ("secondary-1", "thread-1")] {
            serve_holding(&host, id, Holding::Rail);
            switch(&shell, key, id, cx);
            let mut seen = vec![rail_lightness(cx)];
            for _ in 0..30 {
                display_frame(cx);
                seen.push(rail_lightness(cx));
            }
            let rest =
                rail_lightness(cx).map(|card| card.unwrap_or_else(|| panic!("{id}'s cards")));
            // The canvas: under the rail, below its last card and that
            // card's shadow.
            let rail = cx.debug_bounds("chat-rail").expect("the rail");
            let last = RAIL_CARDS
                .iter()
                .filter_map(|card| cx.debug_bounds(format!("rail-card-frame-{card}").leak()))
                .map(|card| card.bottom())
                .fold(px(0.), Pixels::max);
            assert!(rail.bottom() - last > px(40.), "{id}'s cards fill the rail");
            let canvas = cx.update(|window, _| {
                let probe = gpui::point(rail.center().x, rail.bottom() - px(8.))
                    .scale(window.scale_factor());
                [0., 1.].map(|backdrop| luminance_at(window, probe, backdrop))
            });
            for (frame, cards) in seen.iter().enumerate() {
                for ((card, lit), rest) in RAIL_CARDS.iter().zip(cards).zip(&rest) {
                    let Some(lit) = lit else { continue };
                    for (backdrop, name) in ["black", "white"].iter().enumerate() {
                        let floor = canvas[backdrop].min(rest[backdrop]);
                        if lit[backdrop] < floor - 0.005 {
                            failures.push(format!(
                                "{mode:?}, {id}, frame {frame}: the {card} card at {:.4} \
                                 over {name}, under both the canvas {:.4} and the card {:.4}",
                                lit[backdrop], canvas[backdrop], rest[backdrop]
                            ));
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A thread's menu fades in over the sidebar, and out again, from what lies
/// under it to the menu and back, through nothing darker than either, in
/// either theme: its shadow does not show through its fill while the fill
/// is faint, though what fades it is the popover it is drawn in.
#[gpui::test]
fn a_menu_fades_in_and_out_through_nothing_darker(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (_shell, _host, cx) = mount_panes(cx);
    settle_frames(cx);
    let light = |at: gpui::Point<Pixels>, cx: &mut VisualTestContext| {
        cx.update(|window, _| {
            let probe = at.scale(window.scale_factor());
            [0., 1.].map(|backdrop| luminance_at(window, probe, backdrop))
        })
    };
    let row = cx
        .debug_bounds("sidebar-thread-thread-2")
        .expect("thread-2's row")
        .center();
    let (right, plain) = (gpui::MouseButton::Right, gpui::Modifiers::none());
    let menu = |cx: &mut VisualTestContext| cx.debug_bounds("tcode-popup-menu");
    let mut failures = Vec::new();
    for mode in [
        crate::theme::ThemeMode::Light,
        crate::theme::ThemeMode::Dark,
    ] {
        cx.update(|window, cx| crate::theme::change_mode(mode, Some(window), cx));
        settle_frames(cx);
        // Where the menu rests, and what it covers there.
        cx.simulate_mouse_down(row, right, plain);
        cx.simulate_mouse_up(row, right, plain);
        settle_frames(cx);
        let at = menu(cx).expect("thread-2's menu").center();
        let open = light(at, cx);
        cx.simulate_keystrokes("escape");
        settle_frames(cx);
        assert!(menu(cx).is_none(), "the menu closed");
        let under = light(at, cx);

        cx.simulate_mouse_down(row, right, plain);
        cx.simulate_mouse_up(row, right, plain);
        draw(cx);
        let mut seen = Vec::new();
        for _ in 0..30 {
            seen.push(light(at, cx));
            display_frame(cx);
        }
        cx.simulate_keystrokes("escape");
        draw(cx);
        for _ in 0..30 {
            seen.push(light(at, cx));
            display_frame(cx);
        }
        assert_eq!(
            light(at, cx),
            under,
            "the menu left what it covered as it was"
        );
        for (frame, lit) in seen.iter().enumerate() {
            for (backdrop, name) in ["black", "white"].iter().enumerate() {
                let floor = under[backdrop].min(open[backdrop]);
                if lit[backdrop] < floor - 0.005 {
                    failures.push(format!(
                        "{mode:?}, frame {frame}: the menu at {:.4} over {name}, under both \
                         what it covers {:.4} and the menu {:.4}",
                        lit[backdrop], under[backdrop], open[backdrop]
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A lead switched back to with an agent open beside it draws the agent's
/// card on the first frame that shows the lead: the card, what it holds
/// and the lead's column beside it are where and as large as they rest,
/// none of them sliding or growing in. The agent is hosted again in the
/// update that binds the lead, before that frame.
#[gpui::test]
fn a_lead_switched_to_shows_the_agent_beside_it_at_rest(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    let store = store_of(&shell, cx);
    serve_finished(&host, "thread-1");
    switch(&shell, "secondary-1", "thread-1", cx);
    settle_frames(cx);
    serve_finished(&host, "agent-1");
    cx.update(|window, cx| {
        crate::agents_panel::open_agent(
            &store,
            "agent-1".into(),
            crate::shell::side_host_for("thread-1", cx),
            None,
            window,
            cx,
        )
    });
    arrive(&shell, cx);
    settle_frames(cx);
    assert!(
        cx.debug_bounds("chat-side-card").is_some(),
        "agent-1 beside thread-1"
    );

    serve_other(&host);
    switch(&shell, "secondary-2", "thread-2", cx);
    settle_frames(cx);
    assert!(cx.debug_bounds("chat-side-card").is_none());

    serve_finished(&host, "thread-1");
    serve_finished(&host, "agent-1");
    switch(&shell, "secondary-1", "thread-1", cx);
    // The first frame that shows the lead, however little.
    for _ in 0..10 {
        if bubble_fill("thread-1", cx).is_some_and(|fill| fill > 0.) {
            break;
        }
        display_frame(cx);
    }
    let first = (painted(cx), cx.debug_bounds("chat-side-card"));
    settle_frames(cx);
    let rest = (painted(cx), cx.debug_bounds("chat-side-card"));
    assert!(rest.1.is_some(), "agent-1 beside thread-1 again");
    let (moving, resting) = differences(&first.0, &rest.0);
    assert!(
        first == rest,
        "card {:?} -> {:?}; first frame only {moving:?}; at rest only {resting:?}",
        first.1,
        rest.1
    );
}

/// What every conversation row painted in the last frame draws, against its
/// row: each row's size and the quads painted within it, from its origin.
fn rows_painted(cx: &mut VisualTestContext) -> Vec<(usize, [i32; 2], Vec<[i32; 4]>)> {
    let rows: Vec<_> = (0..64)
        .filter_map(|row| {
            let bounds = cx.debug_bounds(format!("timeline-row-{row}").leak())?;
            Some((row, bounds))
        })
        .collect();
    cx.update(|window, _| {
        let quads = crate::test_frames::painted_quads(window);
        rows.into_iter()
            .map(|(row, bounds)| {
                let bounds = bounds.scale(window.scale_factor());
                let mut inside: Vec<[i32; 4]> = quads
                    .iter()
                    .filter(|quad| {
                        within(&quad.bounds, &bounds)
                            && quad
                                .background
                                .as_solid()
                                .is_some_and(|colour| colour.a > 0.)
                    })
                    .map(|quad| {
                        [
                            quad.bounds.origin.x.0 - bounds.origin.x.0,
                            quad.bounds.origin.y.0 - bounds.origin.y.0,
                            quad.bounds.size.width.0,
                            quad.bounds.size.height.0,
                        ]
                        .map(|value| value.round() as i32)
                    })
                    .collect();
                inside.sort_unstable();
                let size = [bounds.size.width.0, bounds.size.height.0].map(|v| v.round() as i32);
                (row, size, inside)
            })
            .collect()
    })
}

/// On a phone, a thread gone back from while its answer was still
/// revealing, and opened again once its turn finished the answer and went
/// on past it, comes in with every message as it rests: none reveals what
/// it gained while the reader was away.
#[gpui::test]
fn a_thread_opened_again_on_a_phone_reveals_nothing_it_gained_away(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    as_mobile(cx);
    cx.simulate_resize(gpui::size(px(393.), px(852.)));
    serve_holding(&host, "thread-1", Holding::StreamedOn);
    settle_frames(cx);
    let live = |event: agent::AgentEvent| {
        (
            Topic::SessionEvents {
                session_id: "thread-1".into(),
            },
            ServerEvent::SessionEvent(event.into()),
        )
    };
    send_events(&host, vec![live(answer_delta("thread-1", streamed_on(1)))]);
    arrive(&shell, cx);
    display_frame(cx);
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx)));
    settle_frames(cx);

    send_events(
        &host,
        vec![
            live(answer_delta("thread-1", streamed_on(2))),
            live(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: "thread-1-next".into(),
                parent_item_id: None,
                content: agent::ItemContent::AssistantMessage {
                    text: "The tests pass, `all of them`.".into(),
                    summarized: false,
                },
            })),
        ],
    );
    arrive(&shell, cx);
    settle_frames(cx);
    store_of(&shell, cx).update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    arrive(&shell, cx);
    // The frame the page lands on: its question stays where it is.
    let mut question = cx.debug_bounds(bubble("thread-1"));
    for _ in 0..60 {
        display_frame(cx);
        let now = cx.debug_bounds(bubble("thread-1"));
        if now.is_some() && now == question {
            break;
        }
        question = now;
    }
    let landed = rows_painted(cx);
    settle_frames(cx);
    let rest = rows_painted(cx);
    assert!(rest.len() > 1, "thread-1's rows: {rest:?}");
    assert_eq!(
        landed, rest,
        "a row still moved or revealed once its page landed"
    );
}
