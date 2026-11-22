use super::*;
use crate::chat::tests::*;
use crate::test_frames::draw_settled;
use agent::ItemStatus;
use gpui::TestAppContext;
use tcode_core::session::TurnMeta;

#[gpui::test]
fn a_pending_steer_shows_as_its_bubble(cx: &mut TestAppContext) {
    use gpui::{VisualTestContext, px, size};
    use tcode_core::session::SteeringStatus;

    let mut timeline = Timeline::default();
    timeline.turns = vec![TurnMeta {
        running: true,
        ..TurnMeta::default()
    }];
    timeline.entries.extend([
        entry("user", user_item("go")),
        entry("assistant", assistant("On it.")),
        entry(
            "steer",
            EntryContent::Steer {
                text: "also do this".into(),
                status: SteeringStatus::Pending,
                attachments: Vec::new(),
            },
        ),
    ]);

    let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (_view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
        ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(1_024.), px(700.)));
    draw_settled(cx);

    assert!(cx.debug_bounds("steering-steer").is_some());
}

/// The host steers its notices into a running turn: steered or still
/// pending, a notice is never the user's bubble. Each child callback is a
/// card of its own, one still pending says so, and a request its child no
/// longer waits on offers no answer and says so, its text whole on demand.
#[gpui::test]
fn steered_notices_render_as_disclosures_not_bubbles(cx: &mut TestAppContext) {
    use gpui::{VisualTestContext, px, size};
    use tcode_core::session::SteeringStatus;

    let steer = |text: String, status| EntryContent::Steer {
        text,
        status,
        attachments: Vec::new(),
    };
    let mut timeline = Timeline::default();
    timeline.turns = vec![TurnMeta {
        running: true,
        ..TurnMeta::default()
    }];
    timeline.entries.extend([
            entry("user", user_item("go")),
            entry(
                "wake",
                steer(
                    tcode_core::pull_request_watch::merged_message(5, "https://x/pull/5"),
                    SteeringStatus::Accepted,
                ),
            ),
            entry("assistant", assistant("On it.")),
            entry(
                "results",
                steer(
                    "[orchestrate] thread a (\"First\") reported.\nResult (reported via report_result):\none\n\n[orchestrate] thread b (\"Second\") failed.\nthe provider closed".into(),
                    SteeringStatus::Pending,
                ),
            ),
            entry(
                "request",
                steer(
                    "[orchestrate] thread c (\"Third\") is waiting for approval: command `rm -rf build` (approval_id: r1). Native options: []. The user answers in the child thread.".into(),
                    SteeringStatus::Accepted,
                ),
            ),
        ]);

    let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (_view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
        ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(1_024.), px(700.)));
    draw_settled(cx);

    assert!(
        cx.debug_bounds("pull-request-update-wake").is_some(),
        "the watch's wake is not its card"
    );
    for card in [
        "callback-card-orchestrate-callback-results-0",
        "callback-card-orchestrate-callback-results-1",
        "callback-card-orchestrate-callback-request-0",
    ] {
        assert!(cx.debug_bounds(card).is_some(), "{card} is not a card");
    }
    for pending in [
        "callback-card-steering-orchestrate-callback-results-0",
        "callback-card-steering-orchestrate-callback-results-1",
    ] {
        assert!(
            cx.debug_bounds(pending).is_some(),
            "{pending} is not marked"
        );
    }
    assert!(
        cx.debug_bounds("callback-card-steering-orchestrate-callback-request-0")
            .is_none()
    );
    assert!(
        cx.debug_bounds("callback-card-gone-orchestrate-callback-request-0")
            .is_some(),
        "a request no child waits on does not say so"
    );
    assert!(
        cx.debug_bounds("callback-approval-orchestrate-callback-request-0-cancel")
            .is_none(),
        "a request no child waits on is offered an answer"
    );
    for bubble in ["steering-wake", "steering-results", "steering-request"] {
        assert!(cx.debug_bounds(bubble).is_none(), "{bubble} is a bubble");
    }

    // The request the parent's model was sent is shown whole on demand.
    let body = "callback-card-body-orchestrate-callback-request-0";
    assert!(cx.debug_bounds(body).is_none());
    let toggle = cx
        .debug_bounds("callback-card-toggle-orchestrate-callback-request-0")
        .expect("a request offers its whole text");
    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    draw_settled(cx);
    assert!(
        cx.debug_bounds(body).is_some(),
        "the request is not shown whole"
    );
}

/// A running command's panel watches the host's screen at the panel's
/// width, over one subscription per width that growth keeps and that
/// collapsing the panel or leaving the thread releases.
#[gpui::test]
fn a_running_command_subscribes_to_the_screen_at_its_width_until_closed(cx: &mut TestAppContext) {
    use gpui::{VisualTestContext, px, size};
    use tcode_protocol::Topic;

    let mut threads = seed_two_threads(cx, Timeline::default(), Timeline::default());
    cx.update(|cx| cx.set_reduce_motion(true));
    // A provider runs the command, and its output grows.
    let turn = scripted_turn(
        &threads.scripts,
        &mut threads.driver,
        &threads.first.clone(),
        agent::ProviderKind::ClaudeCode,
        cx,
    );
    for event in [
        agent::AgentEvent::TurnStarted {
            turn_id: "turn".into(),
        },
        agent::AgentEvent::ItemStarted(agent::ThreadItem {
            id: "run".into(),
            parent_item_id: None,
            content: ItemContent::CommandExecution {
                command: "make".into(),
                input: Vec::new(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::InProgress,
            },
        }),
    ] {
        turn.1.try_send(event).unwrap();
    }
    let TwoThreads {
        store,
        mut driver,
        host,
        first: session_id,
        second: other_id,
        ..
    } = threads;
    driver.wait_until(cx, "the running command", |cx| {
        store.read_with(cx, |store, cx| {
            store
                .with_timeline_for(
                    store.sole_thread().as_deref(),
                    |timeline| timeline.entries.iter().any(|entry| entry.id == "run"),
                    cx,
                )
                .unwrap_or(false)
        })
    });
    let window_state = cx.new(|_| WindowState::default());
    let (_, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
        ChatView::new(
            store.clone(),
            window_state,
            Some(session_id.clone()),
            window,
            cx,
        )
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(1_024.), px(900.)));

    let link = driver.link.clone();
    let output_topics = || {
        link.subscribed_topics()
            .into_iter()
            .filter_map(|topic| match topic {
                Topic::CommandOutput {
                    session_id,
                    item_id,
                    cols,
                } => Some((session_id, item_id, cols)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let watched = |cx: &mut VisualTestContext| {
        store.read_with(cx, |store, cx| {
            store
                .command_output_for(store.sole_thread().as_deref(), "run", cx)
                .and_then(|output| output.watched())
        })
    };
    let frame = |cx: &mut VisualTestContext| {
        store.read_with(cx, |store, cx| {
            store
                .command_output_for(store.sole_thread().as_deref(), "run", cx)
                .and_then(|output| output.frame().cloned())
        })
    };
    let mut settle =
        |cx: &mut VisualTestContext, what: &str, ready: &dyn Fn(&mut VisualTestContext) -> bool| {
            driver.settle_until(cx, what, ready);
        };
    // Open the command, as a reader would.
    settle(cx, "the command row", &|cx| {
        cx.debug_bounds("activity-row-run").is_some()
    });
    if cx.debug_bounds("activity-detail").is_none() {
        let row = cx.debug_bounds("activity-row-run").unwrap();
        cx.simulate_click(row.center(), gpui::Modifiers::default());
    }
    settle(cx, "the panel to measure its width", &|cx| {
        watched(cx).is_some()
    });
    let cols = watched(cx).unwrap();
    let subscribed = vec![(session_id.clone(), "run".to_string(), cols)];
    assert_eq!(output_topics(), subscribed);

    for text in ["10%\n", "100%\n"] {
        turn.1
            .try_send(agent::AgentEvent::Delta {
                item_id: "run".into(),
                kind: agent::DeltaKind::CommandOutput,
                text: text.into(),
            })
            .unwrap();
        settle(cx, "one host turn", &|_| true);
        assert_eq!(output_topics(), subscribed, "growth re-subscribed");
    }
    settle(cx, "the screen after the last growth", &|cx| {
        frame(cx).is_some_and(|frame| frame.cols == cols)
    });
    assert_eq!(output_topics(), subscribed);

    cx.simulate_resize(size(px(600.), px(900.)));
    settle(cx, "the narrower screen", &|cx| {
        watched(cx).is_some_and(|watched| watched < cols)
            && frame(cx).is_some_and(|frame| Some(frame.cols) == watched(cx))
    });
    let narrower = watched(cx).unwrap();
    assert_eq!(
        output_topics(),
        vec![(session_id.clone(), "run".to_string(), narrower)]
    );

    let header = cx.debug_bounds("activity-row-run").expect("command row");
    cx.simulate_click(header.center(), gpui::Modifiers::default());
    settle(cx, "the collapsed panel to release its screen", &|_| {
        output_topics().is_empty()
    });
    let header = cx.debug_bounds("activity-row-run").expect("command row");
    cx.simulate_click(header.center(), gpui::Modifiers::default());
    settle(cx, "the reopened panel", &|_| output_topics().len() == 1);

    store.update(cx, |store, cx| store.show_alone(other_id, cx));
    settle(cx, "leaving the thread", &|_| output_topics().is_empty());
    driver.shutdown(cx, &host);
}

/// The rewind panel a message opens offers exactly the scopes its
/// Harness restores that message in: Claude Code all three of any turn
/// (while its preview is still out); OpenCode the conversation, with or
/// without files, of any turn, never files alone.
#[gpui::test]
fn the_rewind_panel_offers_exactly_the_harness_rewind_modes(cx: &mut TestAppContext) {
    use agent::{ProviderKind, RewindMode};
    use gpui::{Modifiers, VisualTestContext, px, size};

    const ALL: [RewindMode; 3] = [
        RewindMode::FilesAndConversation,
        RewindMode::Conversation,
        RewindMode::Files,
    ];
    for (provider, turn, offered) in [
        (ProviderKind::ClaudeCode, 0, &ALL[..]),
        (ProviderKind::ClaudeCode, 1, &ALL[..]),
        (
            ProviderKind::OpenCode,
            0,
            &[RewindMode::FilesAndConversation, RewindMode::Conversation][..],
        ),
        (
            ProviderKind::OpenCode,
            1,
            &[RewindMode::FilesAndConversation, RewindMode::Conversation][..],
        ),
    ] {
        let mut timeline = Timeline::default();
        timeline.turns = (0..2)
            .map(|index| TurnMeta {
                provider_checkpoint_id: Some(format!("msg_checkpoint_{index}")),
                ..TurnMeta::default()
            })
            .collect();
        timeline.entries.extend([
            at_turn(entry("user-0", user_item("first")), 0),
            at_turn(entry("assistant-0", assistant("One.")), 0),
            at_turn(entry("user-1", user_item("second")), 1),
            at_turn(entry("assistant-1", assistant("Two.")), 1),
        ]);
        // A preview the panel asks for starts a scripted provider.
        let (workspace_store, window_state, session_id, _driver, _scripted) =
            seed_chat_scripted(cx, log_of(&timeline), false, Some(provider), true);
        let (_view, window) =
            crate::chat::tests::add_chat_window_hosting_dialogs(cx, |window, cx| {
                ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
            });
        let window: &mut VisualTestContext = window;
        // The preview's spinner turns while the Harness is asked.
        window.update(|_, cx| cx.set_reduce_motion(true));
        window.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(window);
        // The bubble's actions show while the pointer is over it.
        let bubble = window
            .debug_bounds(["user-bubble-user-0", "user-bubble-user-1"][turn])
            .expect("the user bubble is drawn");
        window.simulate_mouse_move(bubble.center(), None, Modifiers::default());
        draw_settled(window);
        let trigger = window
            .debug_bounds(["rewind-user-0", "rewind-user-1"][turn])
            .unwrap_or_else(|| panic!("{provider:?} turn {turn} has a rewind action"));
        window.simulate_click(trigger.center(), Modifiers::default());
        draw_settled(window);
        assert!(window.debug_bounds("rewind-panel").is_some());
        let shown: Vec<RewindMode> = ALL
            .into_iter()
            .filter(|mode| {
                window
                    .debug_bounds(match mode {
                        RewindMode::FilesAndConversation => "rewind-option-FilesAndConversation",
                        RewindMode::Conversation => "rewind-option-Conversation",
                        RewindMode::Files => "rewind-option-Files",
                    })
                    .is_some()
            })
            .collect();
        assert_eq!(shown, offered, "{provider:?} turn {turn}");
    }
}

/// The panel asks the Harness what each file scope would restore, and
/// offers those scopes only when it names files: a message after which
/// nothing changed can only have its conversation rewound, as Claude
/// Code's own menu hides "Restore code" there.
#[gpui::test]
fn the_rewind_panel_drops_file_scopes_the_preview_finds_nothing_for(cx: &mut TestAppContext) {
    use agent::{
        AgentEvent, ProviderKind, RewindFile, RewindFiles, RewindMode, RewindPreview,
        RewindPreviewResult, SessionCommand,
    };
    use gpui::{Modifiers, VisualTestContext, px, size};

    for (files, offered) in [
        (Vec::new(), vec![RewindMode::Conversation]),
        (
            vec![RewindFile::path("src/main.rs")],
            vec![
                RewindMode::FilesAndConversation,
                RewindMode::Conversation,
                RewindMode::Files,
            ],
        ),
    ] {
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            provider_checkpoint_id: Some("checkpoint-0".into()),
            ..TurnMeta::default()
        }];
        timeline.entries.extend([
            at_turn(entry("user-0", user_item("first")), 0),
            at_turn(entry("assistant-0", assistant("One.")), 0),
        ]);
        let (store, window_state, session_id, mut driver, scripted) = seed_chat_scripted(
            cx,
            log_of(&timeline),
            false,
            Some(ProviderKind::ClaudeCode),
            true,
        );
        let (commands, events) = scripted.unwrap();
        let (_view, window) =
            crate::chat::tests::add_chat_window_hosting_dialogs(cx, |window, cx| {
                ChatView::new(store.clone(), window_state, Some(session_id), window, cx)
            });
        let window: &mut VisualTestContext = window;
        // The preview's spinner turns while the Harness is asked.
        window.update(|_, cx| cx.set_reduce_motion(true));
        window.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(window);
        let bubble = window.debug_bounds("user-bubble-user-0").unwrap();
        window.simulate_mouse_move(bubble.center(), None, Modifiers::default());
        draw_settled(window);
        let trigger = window.debug_bounds("rewind-user-0").unwrap();
        window.simulate_click(trigger.center(), Modifiers::default());
        draw_settled(window);
        assert!(window.debug_bounds("rewind-preview-checking").is_some());

        let mut asked = Vec::new();
        while asked.len() < 2 {
            if let SessionCommand::PreviewRewind {
                checkpoint_id,
                mode,
            } = driver.recv(window, &commands)
            {
                asked.push((checkpoint_id, mode));
            }
        }
        for (checkpoint_id, mode) in asked {
            assert_eq!(checkpoint_id, "checkpoint-0");
            events
                .try_send(AgentEvent::RewindPreviewed {
                    checkpoint_id,
                    mode,
                    result: RewindPreviewResult::Ready(RewindPreview {
                        files: RewindFiles {
                            files: files.clone(),
                            insertions: Some(3),
                            deletions: Some(1),
                            skipped_links: 0,
                        },
                        conflicts: Vec::new(),
                    }),
                })
                .unwrap();
        }
        let shown = |window: &mut VisualTestContext| -> Vec<RewindMode> {
            [
                RewindMode::FilesAndConversation,
                RewindMode::Conversation,
                RewindMode::Files,
            ]
            .into_iter()
            .filter(|mode| {
                window
                    .debug_bounds(format!("rewind-option-{mode:?}").leak())
                    .is_some()
            })
            .collect()
        };
        // The spinner is the selected scope's preview alone; the scopes
        // offered settle once the other scope's answer lands as well.
        driver.settle_until(window, "the previews' answers", |window| {
            window.debug_bounds("rewind-preview-checking").is_none() && shown(window) == offered
        });
        assert_eq!(
            window.debug_bounds("rewind-preview-files").is_some(),
            !files.is_empty(),
            "the files the default scope restores"
        );
    }
}

/// A plan the agent waits on is decided on its block, not in the
/// composer: a choice there answers with that option. Words sent from
/// the composer while one waits answer it instead, as feedback.
#[gpui::test]
fn a_plan_is_decided_on_its_block_and_answered_in_words_from_the_composer(cx: &mut TestAppContext) {
    use gpui::{Modifiers, VisualTestContext, px, size};

    cx.update(|cx| cx.set_reduce_motion(true));
    let (store, window_state, session_id, mut driver, scripted) = seed_chat_scripted(
        cx,
        log_of(&Timeline::default()),
        false,
        Some(agent::ProviderKind::ClaudeCode),
        true,
    );
    let (commands, events) = scripted.expect("the provider is scripted");
    driver.command(
        cx,
        tcode_protocol::Command::SendTurn {
            session_id: session_id.clone(),
            text: "Plan it first".into(),
            attachment_paths: Vec::new(),
        },
    );
    while !matches!(
        driver.recv(cx, &commands),
        agent::SessionCommand::SendTurn { .. }
    ) {}
    let (chat, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
        ChatView::new(
            store.clone(),
            window_state,
            Some(session_id.clone()),
            window,
            cx,
        )
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(1_024.), px(800.)));
    let propose = |id: &str, request: &str| {
        let markdown = format!("# Plan {id}\n\n1. Write hello.txt");
        for event in [
            agent::AgentEvent::ItemStarted(agent::ThreadItem {
                id: id.into(),
                parent_item_id: None,
                content: ItemContent::Plan {
                    markdown: markdown.clone(),
                },
            }),
            agent::AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: request.into(),
                turn_id: None,
                parent_item_id: None,
                kind: agent::ApprovalKind::Plan {
                    item_id: id.into(),
                    markdown,
                },
                reason: None,
                blocked_path: None,
                options: vec![
                    agent::ApprovalOption {
                        id: "allow".into(),
                        label: "Approve".into(),
                        kind: agent::ApprovalOptionKind::AllowOnce,
                    },
                    agent::ApprovalOption {
                        id: "deny".into(),
                        label: "Keep planning".into(),
                        kind: agent::ApprovalOptionKind::RejectOnce,
                    },
                ],
            }),
        ] {
            events.try_send(event).expect("provider event");
        }
    };
    let decided = |driver: &mut crate::store::tests::LocalHostDriver,
                   cx: &mut VisualTestContext| loop {
        if let agent::SessionCommand::RespondApproval {
            request_id,
            decision,
        } = driver.recv(cx, &commands)
        {
            return (request_id, decision);
        }
    };

    propose("plan-1", "decide-1");
    driver.settle_until(cx, "the first plan's decision", |cx| {
        cx.debug_bounds("plan-decision").is_some()
    });
    assert!(
        cx.debug_bounds("approval-header").is_none(),
        "a plan is decided on its block, not in the composer"
    );
    let approve = cx
        .debug_bounds("plan-option-allow")
        .expect("approve button");
    cx.simulate_click(approve.center(), Modifiers::default());
    assert_eq!(
        decided(&mut driver, cx),
        (
            "decide-1".to_string(),
            agent::ApprovalDecision::Option("allow".into())
        )
    );
    // The first plan's choices go once the host has told this window
    // the plan is answered; only then is a decision drawn the second's.
    driver.settle_until(cx, "the first plan answered", |cx| {
        cx.debug_bounds("plan-decision").is_none()
    });

    propose("plan-2", "decide-2");
    driver.settle_until(cx, "the second plan's decision", |cx| {
        cx.debug_bounds("plan-decision").is_some()
    });
    chat.update_in(cx, |chat, window, cx| chat.focus_composer(window, cx));
    cx.simulate_input("Split step one");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        decided(&mut driver, cx),
        (
            "decide-2".to_string(),
            agent::ApprovalDecision::Feedback("Split step one".into())
        )
    );
}

/// A `tcode_control` child's approval and question are answered on its
/// lead's cards for the callbacks, with what its harness offered, and each
/// card then records how it was answered and offers nothing more.
#[gpui::test]
fn a_childs_requests_are_answered_on_its_leads_cards(cx: &mut TestAppContext) {
    use gpui::{Modifiers, VisualTestContext, px, size};

    cx.update(|cx| cx.set_reduce_motion(true));
    let disk = chat_store(cx);
    let lead = thread_meta(None);
    let mut child = thread_meta(None);
    child.parent_session_id = Some(lead.id.clone());
    record_thread(&disk, &lead, []);
    record_thread(&disk, &child, []);
    let scripts = tcode_runtime::app::Scripts::default();
    let host = spawn_chat_host(disk, scripts.launcher());
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    driver.wait_until(cx, "workspace baseline", |cx| {
        store.read_with(cx, |store, _| store.baseline_ready())
    });
    store.update(cx, |store, cx| store.show_alone(lead.id.clone(), cx));
    let _lead_turn = scripted_turn(
        &scripts,
        &mut driver,
        &lead.id,
        agent::ProviderKind::ClaudeCode,
        cx,
    );
    // The child runs where a client opened it.
    store.update(cx, |store, cx| store.open_thread(&child.id, cx));
    driver.wait_until(cx, "the child's status", |cx| {
        store.read_with(cx, |store, cx| {
            store.session_status_for(Some(&child.id), cx).is_some()
        })
    });
    let (child_commands, child_events) = scripted_turn(
        &scripts,
        &mut driver,
        &child.id,
        agent::ProviderKind::ClaudeCode,
        cx,
    );
    child_events
        .try_send(agent::AgentEvent::ApprovalRequested(
            agent::ApprovalRequest {
                id: "ask-1".into(),
                turn_id: None,
                parent_item_id: None,
                kind: agent::ApprovalKind::ExecCommand {
                    command: "rm -rf build".into(),
                    cwd: None,
                    description: None,
                },
                reason: None,
                blocked_path: None,
                options: vec![
                    agent::ApprovalOption {
                        id: "allow".into(),
                        label: "Allow once".into(),
                        kind: agent::ApprovalOptionKind::AllowOnce,
                    },
                    agent::ApprovalOption {
                        id: "deny".into(),
                        label: "Reject".into(),
                        kind: agent::ApprovalOptionKind::RejectOnce,
                    },
                ],
            },
        ))
        .unwrap();
    let window_state = cx.new(|_| crate::window_state::WindowState::default());
    let (_chat, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
        ChatView::new(
            store.clone(),
            window_state,
            Some(lead.id.clone()),
            window,
            cx,
        )
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(1_024.), px(800.)));
    // The key of the lead's `nth` callback card.
    let callback = |nth: usize, cx: &mut VisualTestContext| -> Option<&'static str> {
        store
            .read_with(cx, |store, cx| {
                store.with_timeline_for(
                    Some(&lead.id),
                    |timeline| {
                        timeline
                            .entries
                            .iter()
                            .filter(|entry| {
                                matches!(
                                    user_notice(&entry.content),
                                    Some(HostNotice::Orchestrate(_))
                                )
                            })
                            .nth(nth)
                            .map(|entry| format!("orchestrate-callback-{}-0", entry.id))
                    },
                    cx,
                )
            })
            .flatten()
            .map(|key| &*key.leak())
    };
    driver.settle_until(cx, "the child's approval in the lead", |cx| {
        callback(0, cx).is_some()
    });
    let card = callback(0, cx).unwrap();
    let option: &'static str = format!("callback-approval-{card}-option-allow").leak();
    driver.settle_until(cx, "the child's approval on the lead's card", |cx| {
        cx.debug_bounds(option).is_some()
    });
    let allow = cx.debug_bounds(option).unwrap();
    cx.simulate_click(allow.center(), Modifiers::default());
    loop {
        if let agent::SessionCommand::RespondApproval {
            request_id,
            decision,
        } = driver.recv(cx, &child_commands)
        {
            assert_eq!(
                (request_id.as_str(), decision),
                ("ask-1", agent::ApprovalDecision::Option("allow".into()))
            );
            break;
        }
    }
    let settled: &'static str = format!("callback-card-settled-{card}").leak();
    driver.settle_until(cx, "the approval answered", |cx| {
        cx.debug_bounds(settled).is_some()
    });
    assert!(
        cx.debug_bounds(option).is_none(),
        "an answered approval is offered again"
    );

    child_events
        .try_send(agent::AgentEvent::UserInputRequested {
            request_id: "ask-2".into(),
            questions: vec![agent::UserInputQuestion {
                id: "lang".into(),
                header: "Language".into(),
                question: "Which language?".into(),
                options: vec![
                    agent::UserInputOption {
                        label: "Rust".into(),
                        description: String::new(),
                    },
                    agent::UserInputOption {
                        label: "Go".into(),
                        description: String::new(),
                    },
                ],
                multi_select: false,
                prefill: None,
                optional: false,
            }],
            delivery: agent::UserInputDelivery::Blocking,
        })
        .unwrap();
    driver.settle_until(cx, "the child's question in the lead", |cx| {
        callback(1, cx).is_some()
    });
    let card = callback(1, cx).unwrap();
    let rust: &'static str = format!("callback-card-option-{card}-0").leak();
    driver.settle_until(cx, "the child's question on the lead's card", |cx| {
        cx.debug_bounds(rust).is_some()
    });
    let choose = cx.debug_bounds(rust).unwrap();
    cx.simulate_click(choose.center(), Modifiers::default());
    let answers = loop {
        if let agent::SessionCommand::RespondUserInput {
            request_id,
            answers,
        } = driver.recv(cx, &child_commands)
        {
            assert_eq!(request_id, "ask-2");
            break answers;
        }
    };
    assert_eq!(
        serde_json::Value::Object(answers.clone()),
        serde_json::json!({"lang": "Rust"})
    );
    child_events
        .try_send(agent::AgentEvent::UserInputResolved {
            request_id: "ask-2".into(),
            answers,
        })
        .unwrap();
    let settled: &'static str = format!("callback-card-settled-{card}").leak();
    driver.settle_until(cx, "the question answered", |cx| {
        cx.debug_bounds(settled).is_some()
    });
    assert!(
        cx.debug_bounds(rust).is_none(),
        "an answered question is offered again"
    );
}

/// A finished turn's fork is beside its answer's Copy where the Harness
/// has one, whatever it is called, and never on a user message or a turn
/// still running. Used, it opens a thread that names where it came from.
#[gpui::test]
fn a_finished_turn_offers_its_fork_beside_the_answer(cx: &mut TestAppContext) {
    use agent::{ProviderKind, TurnStatus};
    use gpui::{Modifiers, VisualTestContext, px, size};

    // The running turn's indicator rests only with motion reduced.
    cx.update(|cx| cx.set_reduce_motion(true));
    for (provider, running, offered) in [
        (ProviderKind::ClaudeCode, false, true),
        (ProviderKind::Pi, false, true),
        (ProviderKind::Cursor, false, false),
        (ProviderKind::ClaudeCode, true, false),
    ] {
        let mut timeline = Timeline::default();
        timeline.turns = (0..2)
            .map(|index| TurnMeta {
                provider_checkpoint_id: Some(format!("checkpoint-{index}")),
                status: Some(TurnStatus::Completed),
                ..TurnMeta::default()
            })
            .collect();
        timeline.entries.extend([
            at_turn(entry("user-0", user_item("first")), 0),
            at_turn(entry("assistant-0", assistant("One.")), 0),
        ]);
        if running {
            timeline.turns.truncate(1);
        } else {
            timeline.entries.extend([
                at_turn(entry("user-1", user_item("second")), 1),
                at_turn(entry("assistant-1", assistant("Two.")), 1),
            ]);
        }
        let (store, window_state, session_id, mut driver, scripted) =
            seed_chat_scripted(cx, log_of(&timeline), false, Some(provider), running);
        // The second turn runs: the user sent it and the provider answers.
        let mut user = "user-1".to_owned();
        if let Some((commands, events)) = &scripted {
            driver.command(
                cx,
                tcode_protocol::Command::SendTurn {
                    session_id: session_id.clone(),
                    text: "second".into(),
                    attachment_paths: Vec::new(),
                },
            );
            let delivery_id = loop {
                if let agent::SessionCommand::SendTurn { delivery_id, .. } =
                    driver.recv(cx, commands)
                {
                    break delivery_id;
                }
            };
            for event in [
                agent::AgentEvent::TurnAccepted { delivery_id },
                agent::AgentEvent::TurnStarted {
                    turn_id: "turn-1".into(),
                },
                agent::AgentEvent::ItemStarted(agent::ThreadItem {
                    id: "assistant-1".into(),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: "Two.".into(),
                        summarized: false,
                    },
                }),
            ] {
                events.try_send(event).unwrap();
            }
            let sent = |cx: &TestAppContext| {
                store.read_with(cx, |store, cx| {
                    store
                        .with_timeline_for(
                            Some(session_id.as_str()),
                            |timeline| {
                                let last = timeline.entries.last()?;
                                (last.id == "assistant-1").then(|| {
                                    timeline.entries.iter().rev().find_map(|entry| {
                                        crate::chat::model::user_content(&entry.content)
                                            .map(|_| entry.id.clone())
                                    })
                                })?
                            },
                            cx,
                        )
                        .flatten()
                })
            };
            driver.wait_until(cx, "the running turn", |cx| sent(cx).is_some());
            user = sent(cx).unwrap();
        }
        let (_view, window) =
            crate::chat::tests::add_chat_window_hosting_dialogs(cx, |window, cx| {
                ChatView::new(
                    store.clone(),
                    window_state.clone(),
                    Some(session_id.clone()),
                    window,
                    cx,
                )
            });
        let window: &mut VisualTestContext = window;
        window.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(window);
        let bubble = window
            .debug_bounds(format!("user-bubble-{user}").leak())
            .expect("the user bubble is drawn");
        window.simulate_mouse_move(bubble.center(), None, Modifiers::default());
        draw_settled(window);
        assert!(window.debug_bounds(format!("fork-{user}").leak()).is_none());
        if let Some(copy) = window.debug_bounds("copy-assistant:assistant-1") {
            window.simulate_mouse_move(copy.center(), None, Modifiers::default());
            draw_settled(window);
        }
        assert_eq!(
            window.debug_bounds("fork-assistant-1").is_some(),
            offered,
            "{provider:?} running {running}"
        );
        if !offered {
            continue;
        }
        let copy = window
            .debug_bounds("copy-assistant:assistant-0")
            .expect("the finished turn's answer has its actions");
        window.simulate_mouse_move(copy.center(), None, Modifiers::default());
        draw_settled(window);
        let action = window
            .debug_bounds("fork-assistant-0")
            .expect("the fork is beside Copy");

        window.simulate_click(action.center(), Modifiers::default());
        draw_settled(window);
        assert!(
            window.update(|window, cx| crate::overlay::dialog_open(window, cx)),
            "the fork asks first"
        );
        window.simulate_keystrokes("enter");
        let fork = |cx: &App| {
            store
                .read(cx)
                .sidebar_sessions()
                .iter()
                .find(|meta| meta.fork_origin.is_some())
                .map(|meta| meta.id.clone())
        };
        driver.wait_until(window, "the fork", |cx| cx.update(|cx| fork(cx).is_some()));
        let fork = window.update(|_, cx| fork(cx)).unwrap();
        let fork_id = fork.clone();
        window.update(|_, cx| store.update(cx, |store, cx| store.show_alone(fork_id, cx)));
        let (_view, window) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(store.clone(), window_state, Some(fork.clone()), window, cx)
        });
        let window: &mut VisualTestContext = window;
        window.simulate_resize(size(px(1_024.), px(700.)));
        // The origin comes with the thread's metadata, once its status
        // reaches this window, which redraws it.
        driver.settle_until(window, "the fork's source", |window| {
            window.debug_bounds("fork-origin").is_some()
        });
    }
}

/// A rewind's divider says what the rewind did; a truncating one only
/// lets the removed turns be read back, an undoable one adds Undo, and a
/// branch adds the way back to the branch it left.
#[gpui::test]
fn a_rewind_divider_offers_what_its_harness_keeps(cx: &mut TestAppContext) {
    use agent::{LaterHistory, ProviderKind, RewindMode};
    use gpui::{Modifiers, VisualTestContext, px, size};

    for (later_history, undo_token, sent_after, undo) in [
        (LaterHistory::Truncated, None, false, None),
        (
            LaterHistory::HiddenUntilNextSend,
            Some("revert"),
            false,
            Some("rewound-undo"),
        ),
        (
            LaterHistory::HiddenUntilNextSend,
            Some("revert"),
            true,
            None,
        ),
        (
            LaterHistory::Branch,
            Some("/sessions/original.jsonl"),
            false,
            Some("rewound-return"),
        ),
        (
            LaterHistory::Branch,
            Some("/sessions/original.jsonl"),
            true,
            Some("rewound-open-branch"),
        ),
    ] {
        let mut events = Vec::new();
        for index in 0..2 {
            events.extend([
                agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: format!("user-{index}"),
                    parent_item_id: None,
                    content: agent::ItemContent::UserMessage {
                        text: format!("prompt {index}"),
                        attachments: Vec::new(),
                    },
                }),
                agent::AgentEvent::TurnCheckpoint {
                    turn_id: format!("turn-{index}"),
                    checkpoint_id: format!("checkpoint-{index}"),
                },
                agent::AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{index}"),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                },
            ]);
        }
        events.push(agent::AgentEvent::RewindCompleted {
            checkpoint_id: "checkpoint-1".into(),
            mode: RewindMode::Conversation,
            prefill: None,
            prefill_attachments: Vec::new(),
            later_history,
            files: None,
            incomplete: None,
            undo_token: undo_token.map(str::to_owned),
        });
        if sent_after {
            events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: "user-after".into(),
                parent_item_id: None,
                content: agent::ItemContent::UserMessage {
                    text: "sent after".into(),
                    attachments: Vec::new(),
                },
            }));
        }
        let (workspace_store, window_state, session_id, _driver) =
            seed_chat_log_on(cx, events, Some(ProviderKind::OpenCode));
        cx.update(|cx| cx.set_reduce_motion(true));
        let (_view, window) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(
                workspace_store.clone(),
                window_state,
                Some(session_id),
                window,
                cx,
            )
        });
        let window: &mut VisualTestContext = window;
        window.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(window);
        // The divider the client folded from the host's records.
        let divider = workspace_store
            .read_with(window, |store, cx| {
                store.with_timeline_for(
                    store.sole_thread().as_deref(),
                    |timeline| {
                        timeline
                            .entries
                            .iter()
                            .rev()
                            .find(|entry| matches!(entry.content, EntryContent::Rewound(_)))
                            .map(|entry| entry.id.clone())
                    },
                    cx,
                )
            })
            .flatten()
            .expect("the rewind's divider");
        let toggle = window
            .debug_bounds(format!("rewound-toggle-{divider}").leak())
            .unwrap_or_else(|| panic!("{later_history:?}: the removed turn can be read"));
        for action in ["rewound-undo", "rewound-return", "rewound-open-branch"] {
            assert_eq!(
                window
                    .debug_bounds(format!("{action}-{divider}").leak())
                    .is_some(),
                undo == Some(action),
                "{later_history:?}: {action}"
            );
        }
        assert!(
            window
                .debug_bounds(format!("rewound-removed-{divider}").leak())
                .is_none(),
            "the removed turn is folded"
        );
        window.simulate_click(toggle.center(), Modifiers::default());
        draw_settled(window);
        assert!(
            window
                .debug_bounds(format!("rewound-removed-{divider}").leak())
                .is_some()
        );
    }
}
