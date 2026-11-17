use super::*;

/// Compacting or clearing the goal goes to the harness only between turns;
/// over a running turn the command is refused.
#[test]
fn compaction_waits_for_the_running_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-compact-data");
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    let compact = || Command::CompactContext {
        session_id: "thread".into(),
    };
    let clear = || Command::ClearGoal {
        session_id: "thread".into(),
    };
    assert!(state.command(cx, compact()).is_err());
    assert!(state.command(cx, clear()).is_err());
    assert!(next_command(&provider.commands).is_err());

    provider.play([completed_turn("turn")]);
    cx.run_until_seen(|view| view.status("thread").running_turn.is_none());
    state.command(cx, compact()).unwrap();
    assert!(matches!(provider.next_command(cx), SessionCommand::Compact));
    state.command(cx, clear()).unwrap();
    assert!(matches!(
        provider.next_command(cx),
        SessionCommand::ClearGoal
    ));
    state
        .command(
            cx,
            Command::UpdateProfileSettings {
                profile_id: "claude".into(),
                patch: ProfileSettingsPatch::SetEnv(vec![tcode_core::settings::EnvVar {
                    name: "BASE_URL".into(),
                    value: "https://compaction.invalid".into(),
                    sensitive: false,
                }]),
            },
        )
        .unwrap();
    state.command(cx, compact()).unwrap();
    assert!(matches!(
        provider.next_command(cx),
        SessionCommand::Shutdown
    ));
    let restarted = providers.started(cx, 1);
    assert!(matches!(
        restarted.next_command(cx),
        SessionCommand::Compact
    ));
    assert!(
        restarted
            .options
            .launch_env
            .env
            .contains(&("BASE_URL".into(), "https://compaction.invalid".into()))
    );
    assert!(next_command(&provider.commands).is_err());
}

/// A client that leaves one thread for another before the first's log has
/// reached it is sent only the second's.
#[test]
fn stale_timeline_completion_cannot_land_on_another_session() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-stale-timeline-load-test");
    for (id, text) in [("a", "only session A"), ("b", "only session B")] {
        stored_thread(
            &test_store,
            id,
            ProviderKind::Codex,
            &test_store.root().join(id),
            |_| {},
        );
        test_store
            .append_event(id, 1, &persisted_assistant_event(text))
            .unwrap();
    }
    let (state, _providers) = played_host(cx, &test_store);
    let events = |id: &str| {
        tcode_protocol::ClientPayload::Subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionEvents {
                session_id: id.into(),
            },
            after: None,
        })
    };
    state.deliver(cx, 1, events("a"));
    state.deliver(
        cx,
        2,
        tcode_protocol::ClientPayload::Unsubscribe(Subscription {
            known_index: None,
            topic: Topic::SessionEvents {
                session_id: "a".into(),
            },
            after: None,
        }),
    );
    state.open(cx, "b");
    let texts = |cx: &TestAppContext, id: &str| {
        cx.seen()
            .timeline(id)
            .entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::AssistantMessage { text, .. }) => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(texts(cx, "b"), ["only session B"]);
}

/// After stopping one thread, the next thread's first accepted message must
/// remain visible and durable without inheriting the interrupted thread's error.
#[test]
fn stop_then_new_thread_keeps_the_first_message_visible() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-t3-data");
    let cwd = test_store.root().join("project");
    let project = tcode_core::project::Project::from_root(cwd.clone());
    test_store.upsert_project(&project).unwrap();
    stored_thread(&test_store, "a", ProviderKind::ClaudeCode, &cwd, |meta| {
        meta.project_id = Some(project.id.clone());
    });
    let (state, providers) = played_host(cx, &test_store);
    let a = first_turn(&state, cx, &providers, "a", 0);
    // Stop. The provider reports an error and an interrupted turn.
    state
        .command(
            cx,
            Command::Interrupt {
                session_id: "a".into(),
            },
        )
        .unwrap();
    assert!(matches!(a.next_command(cx), SessionCommand::Interrupt));
    a.play([
        AgentEvent::Error {
            message: "Request was aborted\nwith a second line the toast never showed".into(),
            fatal: false,
        },
        turn_completed("turn", TurnStatus::Interrupted),
    ]);
    cx.run_until_seen(|view| has_record(view, "a", is_turn_end));

    // Immediately: a new thread, and its first message.
    let Ok(CommandResponse::SessionId(Some(b))) = state.command(
        cx,
        Command::StartDraft {
            project_id: project.id.clone(),
            cwd: cwd.clone(),
        },
    ) else {
        panic!("a new thread")
    };
    assert_ne!(b, "a");
    state.open(cx, &b);
    state.send(cx, &b, "second message");
    let second = providers.started(cx, 1);
    let (delivery_id, _) = second.next_turn(cx);
    second.play([AgentEvent::TurnAccepted { delivery_id }]);
    cx.run_until_seen(|view| !view.timeline(&b).entries.is_empty());
    let timeline = cx.seen().timeline(&b);
    let users: Vec<&str> = timeline
        .entries
        .iter()
        .filter_map(|entry| match &entry.content {
            EntryContent::Item(ItemContent::UserMessage { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(users, ["second message"]);
    let entry_turn = timeline.entries[0].turn;
    assert!(
        entry_turn < timeline.turns.len(),
        "user entry must belong to a rendered turn (turn {entry_turn} of {})",
        timeline.turns.len()
    );
    assert!(
        !timeline
            .entries
            .iter()
            .any(|entry| matches!(entry.content, EntryContent::Error { .. })),
        "session A's interrupt error leaked into the new thread"
    );
    // And it is durable: a replay of the log shows the same thing.
    let replayed = Timeline::fold_events(test_store.read_events(&b).unwrap());
    assert!(replayed.entries.iter().any(|entry| matches!(
        &entry.content,
        EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "second message"
    )));
}

/// A user message is activity: once the provider accepts it the thread's
/// updated time moves, which is what lifts it in the threads list.
#[test]
fn an_accepted_user_message_moves_the_thread_updated_time() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-message-activity-data");
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |meta| {
            meta.title = "Tidy the parser".into();
            meta.updated_at = 100;
        },
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    state.open(cx, "thread");
    state.send(cx, "thread", "one more change");
    let provider = providers.started(cx, 0);
    let (delivery_id, _) = provider.next_turn(cx);
    assert_eq!(cx.seen().session("thread").unwrap().updated_at, 100);
    provider.play([AgentEvent::TurnAccepted { delivery_id }]);
    let now = cx.now_secs();
    cx.run_until_seen(|view| view.session("thread").unwrap().updated_at == now);
    assert_eq!(test_store.load_index().unwrap()[0].updated_at, now);
}

/// A message the user takes back while the provider is already taking it
/// cannot keep the thread working once the turn it opened ends.
#[test]
fn submitted_queue_head_cannot_leak_delivery_after_turn_completion() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-submitted-drop-data");
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    state.open(cx, "thread");
    state.send(cx, "thread", "finish this task");
    let provider = providers.started(cx, 0);
    let (delivery_id, _) = provider.next_turn(cx);
    // The submitted head remains in the visible queue strip until its
    // provider acknowledgement. Its ✕ must not invalidate correlation.
    let _ = state.command(
        cx,
        Command::DropQueued {
            session_id: "thread".into(),
            id: delivery_id,
        },
    );
    provider.play([
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        completed_turn("turn-1"),
    ]);
    cx.run_until_seen(|view| has_record(view, "thread", is_turn_end));
    let running = |cx: &TestAppContext| cx.seen().activity("thread").unwrap().turn_running;
    assert!(!running(cx));
    state.leave(cx, "thread");
    assert!(!running(cx));
    state.open(cx, "thread");
    state.leave(cx, "thread");
    assert!(
        !running(cx),
        "the completed delivery became Working again after reparking"
    );
}

/// Only the event stream of the provider a thread runs ends that thread's
/// provider, shown or not: its turn ends and the close is recorded. A stream
/// left over from a provider the thread has since replaced changes nothing.
#[test]
fn event_stream_end_closes_only_its_own_live_provider_in_any_residency() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-stream-ownership");
    let cases = [(false, true), (false, false), (true, true), (true, false)];
    for (parked, current) in cases {
        let id = format!("session-{parked}-{current}");
        stored_thread(
            &store,
            &id,
            ProviderKind::ClaudeCode,
            &store.root().join(&id),
            |meta| {
                meta.model = Some("claude-opus-4-8".into());
            },
        );
    }
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    let mut launches = 0;
    for (parked, current) in cases {
        let id = format!("session-{parked}-{current}");
        let id = id.as_str();
        let mut provider = first_turn(&state, cx, &providers, id, launches);
        launches += 1;
        let ending = if current {
            provider.clone()
        } else {
            // The thread takes another model: its provider is replaced, and
            // the old one's stream ends afterwards.
            state
                .command(
                    cx,
                    Command::SetActiveModel {
                        session_id: id.into(),
                        provider: ProviderKind::ClaudeCode,
                        model: Some("claude-sonnet-4-6".into()),
                        profile_id: None,
                    },
                )
                .unwrap();
            state.send(cx, id, "on the new model");
            provider.play([completed_turn("turn")]);
            assert!(matches!(
                provider.next_command(cx),
                SessionCommand::ApplySettings { .. }
            ));
            provider.play([AgentEvent::SettingsApplied { applied: false }]);
            assert!(matches!(
                provider.next_command(cx),
                SessionCommand::Shutdown
            ));
            let old = provider;
            provider = providers.started(cx, launches);
            launches += 1;
            provider.start_turn(cx, "new");
            cx.run_until_seen(|view| view.status(id).running_turn.is_some());
            old
        };
        provider.play([AgentEvent::BackgroundTasksChanged { count: 2 }]);
        provider.fence(cx, id);
        if parked {
            state.leave(cx, id);
        }
        ending.end_stream();
        if current {
            cx.run_until_seen(|view| !view.activity(id).unwrap().turn_running);
        } else {
            // What the replacement says after the old stream ended reaches
            // the thread, so the end has been taken in.
            state.open(cx, id);
            provider.fence(cx, id);
            assert!(cx.seen().activity(id).unwrap().turn_running);
        }
        let closes = store
            .read_events(id)
            .unwrap()
            .iter()
            .filter(|stored| matches!(stored.event, AgentEvent::SessionClosed { .. }))
            .count();
        assert_eq!(
            closes,
            usize::from(current),
            "only the owning stream may persist a close: parked={parked}"
        );
    }
}

/// A thread's activity reads the same whether a client shows it or not:
/// working while its own turn runs, its message is being taken or waits for
/// the provider, and background work counted apart; a turn a gone process
/// left open in its log is not working.
#[test]
fn a_threads_activity_reads_the_same_viewed_or_parked() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-working-location-test");
    let ids = [
        "idle",
        "turn",
        "delivery",
        "queued",
        "background",
        "stale-timeline",
    ];
    for id in ids {
        stored_thread(
            &test_store,
            id,
            ProviderKind::ClaudeCode,
            &test_store.root().join(id),
            |_| {},
        );
    }
    test_store
        .append_event(
            "stale-timeline",
            1,
            &AgentEvent::TurnStarted {
                turn_id: "stale".into(),
            },
        )
        .unwrap();
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let mut launches = 0;
    for (id, working, background) in [
        ("idle", false, false),
        ("turn", true, false),
        ("delivery", true, true),
        ("queued", true, false),
        ("background", false, true),
        ("stale-timeline", false, false),
    ] {
        match id {
            "stale-timeline" => state.open(cx, id),
            "queued" => {
                // The provider takes its time to start.
                providers.hold();
                state.open(cx, id);
                state.send(cx, id, "waiting");
                providers.started(cx, launches);
                launches += 1;
            }
            _ => {
                let provider = first_turn(&state, cx, &providers, id, launches);
                launches += 1;
                match id {
                    "turn" => {}
                    "delivery" => {
                        provider.play([
                            AgentEvent::BackgroundTasksChanged { count: 1 },
                            completed_turn("turn"),
                        ]);
                        cx.run_until_seen(|view| view.status(id).running_turn.is_none());
                        state.send(cx, id, "being taken");
                        provider.next_turn(cx);
                    }
                    "background" => provider.play([
                        AgentEvent::BackgroundTasksChanged { count: 1 },
                        completed_turn("turn"),
                    ]),
                    _ => provider.play([completed_turn("turn")]),
                }
                provider.fence(cx, id);
            }
        }
        let read = |cx: &TestAppContext| {
            let activity = cx.seen().activity(id).unwrap();
            (activity.working, activity.background.tasks > 0)
        };
        assert_eq!(read(cx), (working, background), "{id}");
        assert_eq!(cx.seen().status(id).activity.working, working, "{id}");
        state.leave(cx, id);
        assert_eq!(read(cx), (working, background), "{id} parked");
        providers.let_go();
    }
}

/// A running grandchild keeps every thread above it running an agent, not
/// only its own lead, though none of them has work of its own.
#[test]
fn a_running_grandchild_keeps_every_ancestor_running_an_agent() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-waiting-child-test");
    for (id, parent) in [
        ("parent", None),
        ("child", Some("parent")),
        ("grandchild", Some("child")),
    ] {
        stored_thread(
            &test_store,
            id,
            ProviderKind::ClaudeCode,
            &test_store.root().join(id),
            |meta| {
                meta.parent_session_id = parent.map(Into::into);
            },
        );
    }
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    first_turn(&state, cx, &providers, "grandchild", 0);
    state.leave(cx, "grandchild");
    state.open(cx, "parent");
    let activity = |id: &str| cx.seen().activity(id).unwrap().clone();
    assert!(activity("parent").background.agents == 1 && !activity("parent").working);
    assert!(activity("child").background.agents == 1 && !activity("child").working);
    assert!(activity("grandchild").background.is_idle() && activity("grandchild").working);
}

/// Switching to another thread must not kill a session whose turn is still
/// running. The session parks in the background — process and queue alive,
/// events still recorded, sidebar still "Working" — and selecting it again
/// re-adopts it with the streamed-while-parked content visible.
#[test]
fn switching_threads_parks_a_working_session_instead_of_killing_it() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-park-data");
    stored_thread(
        &test_store,
        "a",
        ProviderKind::ClaudeCode,
        &test_store.root().join("a"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let a = first_turn(&state, cx, &providers, "a", 0);
    state.send(cx, "a", "queued follow-up");

    // Glance at another thread: the session must survive, not die.
    state.leave(cx, "a");
    assert!(
        next_command(&a.commands).is_err(),
        "switching threads must not send Shutdown to a working session"
    );
    assert!(
        cx.seen().activity("a").unwrap().turn_running,
        "a parked working session keeps its sidebar Working status"
    );

    // The parked session keeps streaming, and its turn completes in the
    // background: the queued follow-up goes out as the next turn, on the
    // same process.
    a.play([
        AgentEvent::ItemCompleted(ThreadItem {
            id: "bg-1".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "Migration step 1 done.".into(),
                summarized: false,
            },
        }),
        completed_turn("turn"),
    ]);
    let (delivery_id, text) = a.next_turn(cx);
    assert_eq!(text, "queued follow-up");
    a.play([AgentEvent::TurnAccepted { delivery_id }]);
    cx.run_until_seen(|view| view.activity("a").unwrap().turn_running);

    // Coming back: everything that happened while parked is there, and the
    // turn still runs.
    state.open(cx, "a");
    let timeline = cx.seen().timeline("a");
    assert!(timeline.entries.iter().any(|entry| matches!(
        &entry.content,
        EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "Migration step 1 done."
    )));
    assert!(timeline.entries.iter().any(|entry| matches!(
        &entry.content,
        EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "queued follow-up"
    )));
    a.play([completed_turn("turn-2")]);
    cx.run_until_seen(|view| !view.activity("a").unwrap().turn_running);
}

/// A failed provider start must not destroy what the user typed: the queued
/// message stays in the queue instead of being cleared, the failure is on the
/// record, and it names the Harness and its account.
#[test]
fn failed_provider_start_keeps_the_queued_message() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-t3f-data");
    let cwd = test_store.root().join("project");
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::ClaudeCode,
        &cwd,
        |_| {},
    );
    let mut settings = Settings::default();
    settings.provider_mut(ProviderKind::ClaudeCode).display_name = Some("work".into());
    SettingsStore::new(test_store.root().clone())
        .save(&settings)
        .unwrap();
    let (state, providers) = played_host(cx, &test_store);
    providers.refuse_in(&cwd, "the harness would not start".into());
    state.open(cx, "thread");
    state.send(cx, "thread", "do not lose me");
    cx.run_until_seen(|view| {
        view.timeline("thread")
            .entries
            .iter()
            .any(|entry| matches!(entry.content, EntryContent::ProviderStartError { .. }))
    });
    assert_eq!(
        cx.seen()
            .status("thread")
            .queued_messages
            .first()
            .map(|message| message.text.as_str()),
        Some("do not lose me"),
        "the user's text must survive a failed provider start"
    );
    assert!(
        cx.seen().notifications().iter().any(|notification| matches!(
            notification,
            RuntimeEvent::Error(RuntimeError::ProviderStart { harness, .. }) if harness == "Claude Code (work)"
        )),
        "the failure names the Harness and its account"
    );
}

#[test]
fn a_message_after_a_restart_mid_turn_runs_a_turn_of_its_own() {
    a_message_after_its_process_went_mid_turn_runs_a_turn_of_its_own(true);
}

#[test]
fn a_message_after_the_process_exits_mid_turn_runs_a_turn_of_its_own() {
    a_message_after_its_process_went_mid_turn_runs_a_turn_of_its_own(false);
}
