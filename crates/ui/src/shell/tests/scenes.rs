//! A pane switched to another thread draws it as a new scene: what the
//! thread before left moving, and what the thread shown now did when it was
//! last on screen, carry nothing into the frame that first draws it.

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
fn serve_running(host: &MountedShell, id: &str, cx: &mut VisualTestContext) {
    let mut status = session_status(id, std::path::Path::new(PANES_CWD));
    status.activity.working = true;
    status.activity.turn_running = true;
    status.running_turn = Some(tcode_core::session::RunningTurn {
        turn: 0,
        started_at: Some(cx.update(|_, cx| crate::time::now_millis(cx))),
    });
    serve_log(host, status, vec![started(), asked(id)]);
}

/// What the host has sent arrives, and a frame is drawn.
fn arrive(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    draw(cx);
}

/// Frames until the window asks for none, then a full frame to read: the
/// scene at rest. Only the end of the motion is read, so each frame comes a
/// whole glide after the one before, as a display that stalled would deliver
/// it: what a switch sets moving lands by the next frame. A running turn's
/// working indicator asks for frames for as long as the turn runs, so a
/// window showing one is at rest once three glides have gone by.
fn at_rest(cx: &mut VisualTestContext) {
    draw(cx);
    for _ in 0..3 {
        cx.run_until_parked();
        if cx.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
            break;
        }
        cx.executor().advance_clock(crate::edge_back::SETTLE);
    }
    draw(cx);
}

/// Shows the thread the sidebar lists `key`'s shortcut against, `id`, with
/// what the host has answered by the frame that first draws it.
fn switch(shell: &Entity<AppShell>, key: &str, id: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes(key);
    arrive(cx);
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
    serve_running(&host, "thread-1", cx);
    await_restore_update(&shell, cx, |store, cx| {
        store.composer_state_for(Some("thread-1"), cx).turn_running
    });
    at_rest(cx);
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
    at_rest(cx);
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
    at_rest(cx);
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
        at_rest(cx);
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
    at_rest(cx);
    let panel = |cx: &mut VisualTestContext| {
        cx.debug_bounds("right-panel")
            .zip(cx.debug_bounds("pane-conversation"))
    };

    for width in [2000., 900.] {
        resize(cx, width);
        at_rest(cx);
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
            at_rest(cx);
            assert_eq!(panel(cx), shown, "{id}'s panel at rest at {width}px");
        }
    }

    // Within one thread, the reader closing the panel still slides it out.
    resize(cx, 2000.);
    at_rest(cx);
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
    at_rest(cx);
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
    at_rest(cx);
    serve_finished(&host, "thread-2");
    switch(&shell, "secondary-2", "thread-2", cx);
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-2", tcode_core::ui::RightTab::PullRequests, cx)
    });
    at_rest(cx);
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
        at_rest(cx);
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

/// How far in `id`'s question's bubble is painted against how it is at
/// rest, on the frame drawn now and the `frames` display frames after it;
/// nothing on a frame that does not draw it.
fn watch(id: &str, rest: f32, frames: usize, cx: &mut VisualTestContext) -> Vec<f32> {
    let mut seen = vec![bubble_fill(id, cx).map_or(0., |fill| fill / rest)];
    for _ in 0..frames {
        display_frame(cx);
        seen.push(bubble_fill(id, cx).map_or(0., |fill| fill / rest));
    }
    seen
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
    at_rest(cx);
    let rest = bubble_fill("thread-1", cx).expect("thread-1's question");
    assert!(rest > 0., "the bubble has a fill");
    (shell, host, cx, rest)
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
        .position(|shown| *shown > 0.)
        .expect("thread-2 appears");
    assert!(
        seen[started..]
            .iter()
            .all(|shown| (shown - 1.).abs() < 0.01),
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
fn serve_holding(host: &MountedShell, id: &str, holding: Holding, cx: &mut VisualTestContext) {
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
            started_at: Some(cx.update(|_, cx| crate::time::now_millis(cx))),
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
        Holding::Rail => with_rail(host, id, &mut status, &mut events),
        Holding::Plain | Holding::RightPanel | Holding::AgentsPanel => {}
    }
    if !running {
        events.push(completed());
    }
    serve_log(host, status, events);
}

/// `id`'s side rail cards: a goal, a plan, and a command its turn ran.
fn with_rail(
    host: &MountedShell,
    id: &str,
    status: &mut tcode_protocol::SessionStatus,
    events: &mut Vec<agent::AgentEvent>,
) {
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
fn serve_streamed_on(host: &MountedShell, id: &str, cx: &mut VisualTestContext) {
    let mut status = session_status(id, std::path::Path::new(PANES_CWD));
    status.activity.working = true;
    status.activity.turn_running = true;
    status.running_turn = Some(tcode_core::session::RunningTurn {
        turn: 0,
        started_at: Some(cx.update(|_, cx| crate::time::now_millis(cx))),
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
/// and a long answer, so nothing of it lies where thread-1's lies, and side
/// rail cards of its own, which replace thread-1's.
fn serve_other(host: &MountedShell) {
    serve_other_after(host, 1);
}

/// [`serve_other`] once its turn has gone on `went_on` times while the pane
/// was away: before that it has neither a goal, a plan nor a command; the
/// first time it gains them and the side rail's cards, and each time after
/// it runs one more command.
fn serve_other_after(host: &MountedShell, went_on: usize) {
    let mut status = session_status("thread-2", std::path::Path::new(PANES_CWD));
    let mut events = vec![
        started(),
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: "thread-2-ask".into(),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: "What is in thread-2, which asks at length? ".repeat(8),
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
    ];
    if went_on > 0 {
        with_rail(host, "thread-2", &mut status, &mut events);
    }
    events.extend((1..went_on).map(|index| {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: format!("thread-2-more-{index}"),
            parent_item_id: None,
            content: agent::ItemContent::CommandExecution {
                command: format!("cargo test --test part-{index}"),
                output: "ok\n".into(),
                exit_code: Some(0),
                status: agent::ItemStatus::Completed,
            },
        })
    }));
    events.push(completed());
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

/// A pane switched from thread-1, holding `holding`, to thread-2 and back
/// draws each as a new scene: on the first frame that shows a thread,
/// everything painted is where and as large as it rests, whatever the thread
/// left was showing and whatever the thread shown had when it was last on
/// screen, an answer it left still revealing included, which the thread
/// finished while away, and whatever it gained while away (thread-2 gains a
/// goal, a plan and a command). Only the thread's fade in, which moves
/// nothing, plays.
fn switched_to_at_rest(holding: Holding, cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    serve_other_after(&host, 0);
    switch(&shell, "secondary-2", "thread-2", cx);
    at_rest(cx);
    serve_holding(&host, "thread-1", holding, cx);
    switch(&shell, "secondary-1", "thread-1", cx);
    at_rest(cx);
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
        at_rest(cx);
    }
    if holding == Holding::StreamedOn {
        // More of the answer streams in, and the pane leaves on the frame
        // after.
        send_events(
            &host,
            vec![(
                Topic::SessionEvents {
                    session_id: "thread-1".into(),
                },
                ServerEvent::SessionEvent(answer_delta("thread-1", streamed_on(1)).into()),
            )],
        );
        arrive(cx);
        display_frame(cx);
    }
    for (key, id) in [("secondary-2", "thread-2"), ("secondary-1", "thread-1")] {
        match (id, holding) {
            // The pane draws what the thread held as it left until the host
            // has sent what it did since.
            ("thread-1", Holding::StreamedOn) => {
                switch(&shell, key, id, cx);
                serve_streamed_on(&host, id, cx);
                arrive(cx);
            }
            ("thread-1", _) => {
                serve_holding(&host, id, holding, cx);
                switch(&shell, key, id, cx);
            }
            // thread-2 went on while the pane was away.
            _ => {
                serve_other_after(&host, 1);
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
        at_rest(cx);
        let rest = (painted(cx), cx.debug_bounds("right-panel"));
        let (moving, resting) = differences(&first.0, &rest.0);
        assert!(
            first == rest,
            "{holding:?}, switched to {id}: panel {:?} -> {:?}; \
             first frame only {moving:?}; at rest only {resting:?}",
            first.1,
            rest.1
        );
    }
}

#[gpui::test]
fn a_switch_from_a_finished_turn_starts_at_rest(cx: &mut TestAppContext) {
    switched_to_at_rest(Holding::Plain, cx);
}

#[gpui::test]
fn a_switch_from_a_turns_steers_starts_at_rest(cx: &mut TestAppContext) {
    switched_to_at_rest(Holding::Steers, cx);
}

#[gpui::test]
fn a_switch_from_an_open_right_panel_starts_at_rest(cx: &mut TestAppContext) {
    switched_to_at_rest(Holding::RightPanel, cx);
}

#[gpui::test]
fn a_switch_from_an_open_terminal_drawer_starts_at_rest(cx: &mut TestAppContext) {
    switched_to_at_rest(Holding::Drawer, cx);
}

#[gpui::test]
fn a_switch_from_an_open_agents_panel_starts_at_rest(cx: &mut TestAppContext) {
    switched_to_at_rest(Holding::AgentsPanel, cx);
}

#[gpui::test]
fn a_switch_from_an_agents_window_starts_at_rest(cx: &mut TestAppContext) {
    switched_to_at_rest(Holding::Subagent, cx);
}

#[gpui::test]
fn a_switch_from_a_streaming_answer_starts_at_rest(cx: &mut TestAppContext) {
    switched_to_at_rest(Holding::Streaming, cx);
}

#[gpui::test]
fn a_switch_back_to_an_answer_that_streamed_on_starts_at_rest(cx: &mut TestAppContext) {
    switched_to_at_rest(Holding::StreamedOn, cx);
}

#[gpui::test]
fn a_switch_from_the_rails_cards_starts_at_rest(cx: &mut TestAppContext) {
    switched_to_at_rest(Holding::Rail, cx);
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
    at_rest(cx);
    serve_finished(&host, "agent-1");
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
    arrive(cx);
    at_rest(cx);
    assert!(
        cx.debug_bounds("chat-side-card").is_some(),
        "agent-1 beside thread-1"
    );

    serve_other(&host);
    switch(&shell, "secondary-2", "thread-2", cx);
    at_rest(cx);
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
    at_rest(cx);
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
    serve_holding(&host, "thread-1", Holding::StreamedOn, cx);
    at_rest(cx);
    let live = |event: agent::AgentEvent| {
        (
            Topic::SessionEvents {
                session_id: "thread-1".into(),
            },
            ServerEvent::SessionEvent(event.into()),
        )
    };
    send_events(&host, vec![live(answer_delta("thread-1", streamed_on(1)))]);
    arrive(cx);
    display_frame(cx);
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx)));
    at_rest(cx);

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
    arrive(cx);
    at_rest(cx);
    store_of(&shell, cx).update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
    arrive(cx);
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
    at_rest(cx);
    let rest = rows_painted(cx);
    assert!(rest.len() > 1, "thread-1's rows: {rest:?}");
    assert_eq!(
        landed, rest,
        "a row still moved or revealed once its page landed"
    );
}

/// The records [`finished_turns`] logs for each turn.
const TURN_RECORDS: usize = 5;

/// thread-2's log of `turns` finished turns, each asked, answered and
/// changing a file of its own.
fn finished_turns(turns: usize) -> Vec<tcode_core::session::StoredEvent> {
    (0..turns)
        .flat_map(|turn| {
            [
                agent::AgentEvent::TurnStarted {
                    turn_id: format!("turn-{turn}"),
                },
                agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: format!("thread-2-ask-{turn}"),
                    parent_item_id: None,
                    content: agent::ItemContent::UserMessage {
                        text: format!("What is in part {turn} of thread-2?"),
                        attachments: vec![],
                    },
                }),
                agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: format!("thread-2-answer-{turn}"),
                    parent_item_id: None,
                    content: agent::ItemContent::AssistantMessage {
                        text: format!("Part {turn} holds a few things.\n\n").repeat(3),
                        summarized: false,
                    },
                }),
                agent::AgentEvent::TurnChangesUpdated {
                    turn_id: format!("turn-{turn}"),
                    changes: agent::file_changes_from_unified_diff(&format!(
                        "diff --git a/part_{turn}.rs b/part_{turn}.rs\n\
                         --- a/part_{turn}.rs\n+++ b/part_{turn}.rs\n\
                         @@ -1 +1 @@\n-fn old() {{}}\n+fn new() {{}}\n",
                    ))
                    .expect("one file section"),
                    completeness: agent::ChangeCompleteness::Exact,
                },
                agent::AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{turn}"),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                },
            ]
        })
        .map(Into::into)
        .collect()
}

/// A thread switched to whose log the host sends after the switch, its
/// latest turns first and the turn before them in the page the thread asks
/// for once it is drawn, as a thread holding more than its window does:
/// the page brings only rows above those on screen, so every turn's footer
/// (its time and its answer's Copy) and its changed files stay as they are
/// drawn, rather than the footer rising from the turn below's and the
/// files folding in where the turn below's fold away.
#[gpui::test]
fn a_page_landing_on_a_thread_switched_to_leaves_its_footers_at_rest(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    switch(&shell, "secondary-2", "thread-2", cx);
    const TURNS: usize = 4;
    let records = finished_turns(TURNS);
    let held_from = TURN_RECORDS;
    send_events(
        &host,
        vec![
            (
                Topic::SessionStatus {
                    session_id: "thread-2".into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(session_status(
                    "thread-2",
                    std::path::Path::new(PANES_CWD),
                ))),
            ),
            (
                Topic::SessionEvents {
                    session_id: "thread-2".into(),
                },
                ServerEvent::SessionSnapshot {
                    total: records.len() as u64,
                    total_turns: TURNS as u64,
                    truncated: false,
                    from: held_from as u64,
                    end: records.len() as u64,
                    records: records[held_from..].to_vec(),
                },
            ),
        ],
    );
    arrive(cx);
    // The thread shows, and asks for the turn before its window.
    let mut page = None;
    for _ in 0..30 {
        page = page.or_else(|| history_page_asked(&host));
        if page.is_some() && fill_of("user-bubble-thread-2-ask-3", cx).is_some_and(|fill| fill > 0.)
        {
            break;
        }
        display_frame(cx);
    }
    let (id, before) = page.expect("thread-2 asks for the page before its window");
    assert_eq!(before, held_from as u64);
    let trailers = |cx: &mut VisualTestContext| {
        (0..TURNS)
            .map(|turn| {
                [
                    format!("copy-assistant:thread-2-answer-{turn}"),
                    format!("changed-file-chip-file-part_{turn}.rs"),
                ]
                .map(|selector| cx.debug_bounds(selector.leak()))
            })
            .collect::<Vec<_>>()
    };
    let shown = trailers(cx);
    let from = held_from - TURN_RECORDS;
    answer_query(
        &host,
        id,
        Ok(tcode_protocol::QueryResponse::SessionHistoryPage {
            records: records[from..held_from].to_vec(),
            from: from as u64,
            end: held_from as u64,
            truncated: false,
        }),
    );
    arrive(cx);
    let mut seen = vec![trailers(cx)];
    for _ in 0..12 {
        display_frame(cx);
        seen.push(trailers(cx));
    }
    at_rest(cx);
    let rest = trailers(cx);
    let turns = store_of(&shell, cx).read_with(cx, |store, cx| {
        store.with_timeline_for(Some("thread-2"), |timeline| timeline.turns.len(), cx)
    });
    assert_eq!(turns, Some(TURNS), "the page landed");
    assert!(
        rest[1..].iter().flatten().all(Option::is_some),
        "the window's footers and changed files: {rest:?}"
    );
    assert_eq!(shown, rest, "the page brought only rows above the window's");
    assert!(
        seen.iter().all(|frame| *frame == rest),
        "a footer or a changed file moved once the page landed: {seen:#?}, at rest {rest:?}"
    );
}

/// The history page the client asked for, by its request id and the
/// record it asked for the page before.
fn history_page_asked(host: &MountedShell) -> Option<(u64, u64)> {
    let mut messages: Vec<_> = host.observed.borrow_mut().drain(..).collect();
    while let Ok(line) = host.outgoing.try_recv() {
        messages.push(decode_client_line(&line).expect("client line"));
    }
    messages
        .into_iter()
        .find_map(|message| match message.payload {
            ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage { before, .. }) => {
                Some((message.id, before))
            }
            _ => None,
        })
}
