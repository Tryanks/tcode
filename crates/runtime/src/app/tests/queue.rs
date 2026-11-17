use super::*;

/// A subagent's report steered into its lead's running turn, the lead's
/// thread not shown anywhere, is recorded under the id the provider was sent,
/// so the provider's answer finds it.
#[test]
fn steering_parked_orchestrator_callback_uses_recorded_id() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-steer-test");
    let fleet = Fleet::new(cx, &test_store, ProviderKind::Codex, &["child-a"], |_| {});
    fleet.state.leave(cx, "parent");
    fleet.child("child-a").play([
        report_result_event("rep-1", "full result"),
        completed_turn("turn-1"),
    ]);
    let SessionCommand::Steer {
        request_id, text, ..
    } = fleet.lead.next_command(cx)
    else {
        panic!("callback did not steer")
    };
    assert!(text.contains("full result"));
    fleet.state.open(cx, "parent");
    assert!(
        cx.seen()
            .timeline("parent")
            .entries
            .iter()
            .any(|entry| matches!(
                &entry.content,
                EntryContent::Steer {
                    text,
                    status: tcode_core::session::SteeringStatus::Pending,
                    ..
                } if entry.id == request_id && text.contains("thread child-a")
            ))
    );
}

/// A user's steer, and a queued message the user steers, are sent under an
/// id their client's request names and recorded under the same id; a message
/// the host is already delivering as a turn is neither steered nor dropped.
#[test]
fn steering_user_and_queue_paths_send_the_same_id_they_record() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-user-steer-id-test");
    stored_thread(
        &test_store,
        "active",
        ProviderKind::Codex,
        &test_store.root().join("active"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let provider = first_turn(&state, cx, &providers, "active", 0);
    let queued = |cx: &TestAppContext| {
        cx.seen()
            .status("active")
            .queued_messages
            .iter()
            .map(|message| (message.id, message.text.clone()))
            .collect::<Vec<_>>()
    };
    let send = |cx: &mut TestAppContext, text: &str, key: &str| {
        state
            .command_keyed(
                cx,
                key,
                Command::SendTurn {
                    session_id: "active".into(),
                    text: text.into(),
                    attachment_paths: Vec::new(),
                },
            )
            .unwrap();
        queued(cx).last().unwrap().0
    };
    let steered = |cx: &TestAppContext, request_id: &str, text: &str| {
        cx.seen().timeline("active").entries.iter().any(|entry| {
            matches!(
                &entry.content,
                EntryContent::Steer {
                    text: steered,
                    status: tcode_core::session::SteeringStatus::Pending,
                    ..
                } if entry.id == request_id && steered == text
            )
        })
    };

    let sentinel = send(cx, "queued sentinel", "sentinel-send");
    state
        .command_keyed(
            cx,
            "steer-send",
            Command::Steer {
                session_id: "active".into(),
                text: "redirect".into(),
                attachment_paths: Vec::new(),
            },
        )
        .unwrap();
    let SessionCommand::Steer { request_id, .. } = provider.next_command(cx) else {
        panic!("user steer command missing")
    };
    assert_eq!(request_id, "local-steer-steer-send");
    cx.run_until_seen(|view| {
        has_record(view, "active", |event| {
            matches!(event, AgentEvent::SteerRequested { .. })
        })
    });
    assert!(steered(cx, &request_id, "redirect"));

    state
        .command(
            cx,
            Command::DropQueued {
                session_id: "active".into(),
                id: sentinel,
            },
        )
        .unwrap();
    let first = send(cx, "first", "first-send");
    let queued_id = send(cx, "queued redirect", "queue-send");
    let third = send(cx, "third", "third-send");
    state
        .command_keyed(
            cx,
            "queue-steer-click",
            Command::SteerQueued {
                session_id: "active".into(),
                id: queued_id,
            },
        )
        .unwrap();
    let SessionCommand::Steer { request_id, .. } = provider.next_command(cx) else {
        panic!("queue-to-steer command missing")
    };
    assert_eq!(request_id, "local-steer-queue-send");
    cx.run_until_seen(|view| {
        has_record(view, "active", |event| {
            matches!(event, AgentEvent::SteerRequested { request_id: recorded, .. } if *recorded == request_id)
        })
    });
    assert_eq!(
        queued(cx),
        [(first, "first".to_string()), (third, "third".to_string())]
    );
    assert!(steered(cx, &request_id, "queued redirect"));

    state
        .command(
            cx,
            Command::DropQueued {
                session_id: "active".into(),
                id: first,
            },
        )
        .unwrap();
    // The turn ends, and the host delivers the next message as a turn.
    provider.play([completed_turn("turn")]);
    let (delivery_id, text) = provider.next_turn(cx);
    assert_eq!((delivery_id, text.as_str()), (third, "third"));
    for command in [
        Command::DropQueued {
            session_id: "active".into(),
            id: third,
        },
        Command::SteerQueued {
            session_id: "active".into(),
            id: third,
        },
    ] {
        let _ = state.command(cx, command);
    }
    assert_eq!(queued(cx)[0].0, third);
    assert!(
        next_command(&provider.commands).is_err(),
        "an unacknowledged delivery cannot be steered twice"
    );
}

/// The runtime routes a steer by its own turn flag, but the provider's
/// `turn/completed` can already be on the wire. The provider then rejects the
/// steer by id, and the text goes out as the next ordinary turn, from whoever
/// sent it, instead of vanishing behind a notice.
#[test]
fn steer_rejected_after_turn_end_is_resent_as_a_new_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-steer-rejected-test");
    for id in ["active", "sender"] {
        stored_thread(
            &test_store,
            id,
            ProviderKind::Codex,
            &test_store.root().join(id),
            |meta| {
                meta.project_id = Some("project".into());
            },
        );
    }
    let (state, providers) = controlled_host(cx, &test_store);
    let active = first_turn(&state, cx, &providers, "active", 0);
    // Another thread's agent steers it.
    first_turn(&state, cx, &providers, "sender", 1)
        .control(
            cx,
            "send_message",
            serde_json::json!({ "thread_ids": ["active"], "text": "redirect", "mode": "steer" }),
        )
        .unwrap();
    let SessionCommand::Steer { request_id, .. } = active.next_command(cx) else {
        panic!("steer command missing")
    };

    active.play([completed_turn("turn-1")]);
    active.fence(cx, "active");
    assert!(
        next_command(&active.commands).is_err(),
        "nothing queued yet"
    );

    active.play([AgentEvent::SteerRejected {
        request_id: request_id.clone(),
    }]);
    let (delivery_id, text) = active.next_turn(cx);
    assert_eq!(text, "redirect");
    assert!(
        !cx.seen()
            .timeline("active")
            .entries
            .iter()
            .any(|entry| entry.id == request_id),
        "the pending steer bubble is gone; the send records its own message"
    );
    active.play([AgentEvent::TurnAccepted { delivery_id }]);
    cx.run_until_seen(|view| {
        view.records("active").iter().any(|record| {
            record.origin == Some(MessageOrigin::Agent)
                && matches!(
                    &record.event,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::UserMessage { text, .. },
                        ..
                    }) if text == "redirect"
                )
        })
    });

    active.play([AgentEvent::SteerRejected {
        request_id: request_id.clone(),
    }]);
    active.fence(cx, "active");
    assert!(
        next_command(&active.commands).is_err(),
        "a rejection for an already resolved steer sends nothing"
    );
}

/// Reports that reach a lead whose provider is still starting wait for it,
/// and wake it with one turn that carries all of them.
#[test]
fn callbacks_racing_provider_start_share_one_wakeup_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-start-test");
    let fleet = Fleet::new(
        cx,
        &test_store,
        ProviderKind::ClaudeCode,
        &["child-a", "child-b"],
        |_| {},
    );
    // The lead's turn ends and its harness exits; reports start it again.
    fleet.lead.play([
        completed_turn("turn"),
        AgentEvent::SessionClosed { reason: None },
    ]);
    cx.run_until_seen(|view| {
        has_record(view, "parent", |event| {
            matches!(event, AgentEvent::SessionClosed { .. })
        })
    });
    fleet.providers.hold();
    for (child, result) in [("child-a", "result a"), ("child-b", "result b")] {
        fleet.child(child).play([
            report_result_event(&format!("{child}-report"), result),
            completed_turn("turn-1"),
        ]);
        fleet.child(child).fence(cx, child);
    }
    let restarted = fleet.providers.started(cx, 3);
    assert_eq!(restarted.options.cwd, fleet.root.join("parent"));
    assert_eq!(fleet.providers.count(), 4, "one start for both");
    fleet.providers.let_go();
    let (delivery_id, text) = restarted.next_turn(cx);
    assert!(
        text.contains("result a") && text.contains("result b"),
        "{text}"
    );
    restarted.play([AgentEvent::TurnAccepted { delivery_id }]);
    restarted.fence(cx, "parent");
    assert!(
        next_command(&restarted.commands).is_err(),
        "one turn wakes it"
    );
}

/// Queued messages go in the order the user last moved them to.
#[test]
fn moved_queue_entries_dispatch_in_their_new_order() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-moved-queue-test");
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    for text in ["first", "second", "third"] {
        state.send(cx, "thread", text);
    }
    let id = |cx: &TestAppContext, text: &str| {
        cx.seen()
            .status("thread")
            .queued_messages
            .iter()
            .find(|message| message.text == text)
            .unwrap()
            .id
    };
    let (first, third) = (id(cx, "first"), id(cx, "third"));
    for (moved, before) in [(third, Some(first)), (first, None)] {
        state
            .command(
                cx,
                Command::MoveQueued {
                    session_id: "thread".into(),
                    id: moved,
                    before,
                },
            )
            .unwrap();
    }
    let mut sent = Vec::new();
    for turn in 0..3 {
        provider.play([completed_turn(&format!("turn-{turn}"))]);
        let (delivery_id, text) = provider.next_turn(cx);
        sent.push(text);
        provider.play([
            AgentEvent::TurnAccepted { delivery_id },
            AgentEvent::TurnStarted {
                turn_id: format!("turn-{}", turn + 1),
            },
        ]);
    }
    assert_eq!(sent, ["third", "second", "first"]);
}

/// A message scheduled for later lets an ordinary one sent after it go
/// first, and stays scheduled once that one is taken.
#[test]
fn future_scheduled_head_does_not_block_ordinary_dispatch_or_acceptance() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scheduled-head-test");
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.open(cx, "thread");
    state
        .command(
            cx,
            Command::ScheduleTurn {
                session_id: "thread".into(),
                text: "later".into(),
                attachment_paths: Vec::new(),
                due: tcode_protocol::TurnDue::AfterSecs(3_600),
            },
        )
        .unwrap();
    state.send(cx, "thread", "now");
    let provider = providers.started(cx, 0);
    let (delivery_id, text) = provider.next_turn(cx);
    assert_eq!(text, "now");
    provider.play([AgentEvent::TurnAccepted { delivery_id }]);
    cx.run_until_seen(|view| view.status("thread").delivery_in_flight.is_none());
    let queued = &cx.seen().status("thread").queued_messages;
    assert_eq!(
        queued
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        ["later"]
    );
    assert!(queued[0].fire_at_unix_secs.is_some());
}

/// A scheduled message shows when it is due and can be edited; steering it
/// sends it now, no longer scheduled or editable; dropping one removes it.
#[test]
fn schedule_status_and_queue_actions_preserve_or_remove_deadlines() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scheduled-status-test");
    stored_thread(
        &test_store,
        "scheduled-active",
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let provider = first_turn(&state, cx, &providers, "scheduled-active", 0);
    provider.play([completed_turn("turn")]);
    cx.run_until_seen(|view| view.status("scheduled-active").running_turn.is_none());
    let schedule = |cx: &mut TestAppContext, text: &str, fire_at: u64| {
        state
            .command(
                cx,
                Command::ScheduleTurn {
                    session_id: "scheduled-active".into(),
                    text: text.into(),
                    attachment_paths: Vec::new(),
                    due: tcode_protocol::TurnDue::AtUnixSecs(fire_at),
                },
            )
            .unwrap();
    };
    let status = |cx: &TestAppContext| cx.seen().status("scheduled-active").clone();
    let fire_at = cx.now_secs() + 3_600;
    schedule(cx, "scheduled", fire_at);
    let queued = status(cx).queued_messages;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].fire_at_unix_secs, Some(fire_at));
    assert!(queued[0].editable);
    let scheduled_id = queued[0].id;

    state
        .command(
            cx,
            Command::SteerQueued {
                session_id: "scheduled-active".into(),
                id: scheduled_id,
            },
        )
        .unwrap();
    let (delivery_id, text) = provider.next_turn(cx);
    assert_eq!(text, "scheduled");
    let status_now = status(cx);
    assert_eq!(status_now.queued_messages.len(), 1);
    assert_eq!(status_now.queued_messages[0].id, delivery_id);
    assert_eq!(status_now.queued_messages[0].fire_at_unix_secs, None);
    assert!(!status_now.queued_messages[0].editable);
    assert_eq!(status_now.delivery_in_flight, Some(delivery_id));

    provider.play([
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: "scheduled".into(),
        },
        completed_turn("scheduled"),
    ]);
    cx.run_until_seen(|view| has_record(view, "scheduled-active", |event| {
        matches!(event, AgentEvent::TurnCompleted { turn_id, .. } if turn_id == "scheduled")
    }));
    schedule(cx, "drop me", cx.now_secs() + 7_200);
    let drop_id = status(cx).queued_messages[0].id;
    state
        .command(
            cx,
            Command::DropQueued {
                session_id: "scheduled-active".into(),
                id: drop_id,
            },
        )
        .unwrap();
    assert!(status(cx).queued_messages.is_empty());
}

/// While the harness retries a failed turn itself, what the user queued
/// waits rather than racing the retry; stopping the retry releases it. With
/// a retry the user starts, the next message goes as usual, and a start the
/// harness refuses clears the offer.
#[test]
fn native_retry_keeps_queue_waiting_and_cancellation_releases_it() {
    for automatic in [true, false] {
        let cx = &mut TestAppContext::default();
        let test_store = TestStore::new("native-retry-queue");
        stored_thread(
            &test_store,
            "retry",
            ProviderKind::Codex,
            &test_store.root().join("retry"),
            |_| {},
        );
        let (state, providers) = played_host(cx, &test_store);
        state.open(cx, "retry");
        state.send(cx, "retry", "Go");
        let provider = providers.started(cx, 0);
        provider.start_turn(cx, "failed");
        if automatic {
            state.send(cx, "retry", "queued user message");
        }
        let retry = agent::TurnRetry {
            turn_id: "failed".into(),
            retry_at: automatic.then(|| cx.now_secs() + 10),
        };
        provider.play([
            AgentEvent::TurnRetry {
                retry: Some(retry.clone()),
            },
            completed_turn("failed"),
        ]);
        cx.run_until_seen(|view| has_record(view, "retry", is_turn_end));
        let status = cx.seen().status("retry");
        assert_eq!(status.activity.working, automatic);
        assert!(!status.activity.turn_running);
        assert_eq!(status.turn_retry, Some(retry));
        assert!(
            next_command(&provider.commands).is_err(),
            "queued text must not race the native retry"
        );
        if automatic {
            state
                .command(
                    cx,
                    Command::Interrupt {
                        session_id: "retry".into(),
                    },
                )
                .unwrap();
            assert!(matches!(
                provider.next_command(cx),
                SessionCommand::Interrupt
            ));
            provider.play([AgentEvent::TurnRetry { retry: None }]);
        } else {
            state.send(cx, "retry", "queued user message");
        }
        let (delivery_id, text) = provider.next_turn(cx);
        assert_eq!(text, "queued user message");
        // A rejected new start has no TurnStarted to clear the previous offer.
        provider.play([
            AgentEvent::TurnAccepted { delivery_id },
            turn_completed("", TurnStatus::Failed),
        ]);
        cx.run_until_seen(|view| {
            view.records("retry")
                .iter()
                .filter(|record| is_turn_end(&record.event))
                .count()
                == 2
        });
        let status = cx.seen().status("retry");
        assert!(status.turn_retry.is_none());
        assert!(!status.activity.working);
        assert!(
            state
                .command(
                    cx,
                    Command::RetryTurn {
                        session_id: "retry".into(),
                        turn_id: "failed".into(),
                    },
                )
                .is_err()
        );
    }
}

/// A usage limit the harness reports queues the resume prompt for when the
/// limit resets, unless the user turned that off, and is told to the window
/// once for the account.
#[test]
fn usage_limit_event_schedules_resume_when_enabled_only() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-usage-limit-resume-test");
    for id in ["resume-enabled", "resume-disabled"] {
        stored_thread(
            &test_store,
            id,
            ProviderKind::ClaudeCode,
            &test_store.root().join(id),
            |_| {},
        );
    }
    let (state, providers) = played_host(cx, &test_store);
    let resets_at = cx.now_secs() + 3_600;
    for (n, id) in ["resume-enabled", "resume-disabled"]
        .into_iter()
        .enumerate()
    {
        if n == 1 {
            state
                .command(
                    cx,
                    Command::PatchSettings {
                        patch: SettingsPatch::ResumeOnLimitReset(false),
                    },
                )
                .unwrap();
        }
        let provider = first_turn(&state, cx, &providers, id, n);
        provider.play([AgentEvent::UsageLimitReached { resets_at }]);
        provider.fence(cx, id);
    }
    // Both threads run on one account, whose window is told once.
    let told = cx
        .drain_outgoing()
        .into_iter()
        .filter(|message| {
            matches!(
                message,
                HostMessage::Event(EventEnvelope {
                    event: ServerEvent::Runtime(tcode_protocol::RuntimeNotification::Notice(
                        RuntimeNotice::UsageLimitReached { .. }
                    )),
                    ..
                })
            )
        })
        .count();
    assert_eq!(told, 1);
    let queued = &cx.seen().status("resume-enabled").queued_messages;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].text, tcode_core::session::RESUME_PROMPT);
    assert_eq!(queued[0].fire_at_unix_secs, Some(resets_at));
    assert!(
        cx.seen()
            .status("resume-disabled")
            .queued_messages
            .is_empty()
    );
}

/// A scheduled message goes out as an ordinary message when it comes due.
#[test]
fn due_scheduled_message_reenters_the_ordinary_send_path() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scheduled-fire-test");
    stored_thread(
        &test_store,
        "due-active",
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.open(cx, "due-active");
    state
        .command(
            cx,
            Command::ScheduleTurn {
                session_id: "due-active".into(),
                text: "due now".into(),
                attachment_paths: Vec::new(),
                due: tcode_protocol::TurnDue::AfterSecs(60),
            },
        )
        .unwrap();
    cx.clock.advance(Duration::from_secs(60));
    assert_eq!(providers.started(cx, 0).next_turn(cx).1, "due now");
}
