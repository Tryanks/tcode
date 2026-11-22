use super::*;

/// Same provider, another profile: a different backend. Choosing it waits
/// for the user to confirm handing the conversation over, and binds the
/// thread to that profile for its next launch; choosing the original back
/// needs no handover.
#[test]
fn profile_switch_within_one_provider_requires_a_relay() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-profile-relay-test");
    stored_thread(
        &test_store,
        "profile-relay",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |meta| {
            meta.model = Some("claude-opus-5".into());
        },
    );
    for (ts, event) in [
        AgentEvent::ItemCompleted(ThreadItem {
            id: "user-1".into(),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: "hello".into(),
                attachments: Vec::new(),
            },
        }),
        completed_turn("turn-1"),
    ]
    .iter()
    .enumerate()
    {
        test_store
            .append_event("profile-relay", ts as u64, event)
            .unwrap();
    }
    let (state, _providers) = played_host(cx, &test_store);
    state.open(cx, "profile-relay");
    let choose = |cx: &mut TestAppContext, model: &str, profile_id: Option<&str>| {
        state
            .command(
                cx,
                Command::SetActiveModel {
                    session_id: "profile-relay".into(),
                    provider: ProviderKind::ClaudeCode,
                    model: Some(model.into()),
                    profile_id: profile_id.map(Into::into),
                },
            )
            .unwrap();
    };
    choose(cx, "kimi-k3", Some("kimi"));
    let status = cx.seen().status("profile-relay");
    assert_eq!(status.requested_profile_id.as_deref(), Some("kimi"));
    assert_eq!(status.requested_model.as_deref(), Some("kimi-k3"));
    assert_eq!(
        status.relay_confirmation,
        Some(("Claude Code".into(), "kimi".into()))
    );

    choose(cx, "claude-opus-5", None);
    let status = cx.seen().status("profile-relay");
    assert_eq!(status.requested_profile_id, None);
    assert!(status.relay_confirmation.is_none());
}

/// Switching provider while the only turn is still streaming keeps what it
/// produced: the switch arms a relay instead of silently dropping the cursor,
/// and confirming it closes the abandoned turn so the handoff opens a new one.
/// A scheduled message that comes due meanwhile waits for the confirmation and
/// follows the message that carries the handoff, which goes out as the
/// timeline discloses it and alone carries the conversation.
#[test]
fn provider_switch_mid_turn_relays_the_partial_answer() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-mid-turn-relay-test");
    stored_thread(
        &test_store,
        "mid-turn-relay",
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.open(cx, "mid-turn-relay");
    state.send(cx, "mid-turn-relay", "compare them");
    let codex = providers.started(cx, 0);
    codex.start_turn(cx, "codex-turn");
    codex.play([AgentEvent::ItemStarted(ThreadItem {
        id: "assistant-1".into(),
        parent_item_id: None,
        content: ItemContent::AssistantMessage {
            text: "They differ in".into(),
            summarized: false,
        },
    })]);
    codex.fence(cx, "mid-turn-relay");
    state
        .command(
            cx,
            Command::ScheduleTurn {
                session_id: "mid-turn-relay".into(),
                text: "check the logs".into(),
                attachment_paths: Vec::new(),
                due: tcode_protocol::TurnDue::AfterSecs(0),
            },
        )
        .unwrap();
    state
        .command(
            cx,
            Command::SetActiveModel {
                session_id: "mid-turn-relay".into(),
                provider: ProviderKind::ClaudeCode,
                model: Some("claude-opus-5-5".into()),
                profile_id: None,
            },
        )
        .unwrap();
    let queued = |cx: &TestAppContext| {
        cx.seen()
            .status("mid-turn-relay")
            .queued_messages
            .iter()
            .map(|message| message.text.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        cx.seen().status("mid-turn-relay").relay_confirmation,
        Some(("Codex".into(), "Claude Code".into()))
    );
    // A message that comes due while the relay waits waits with it.
    assert_eq!(queued(cx), ["check the logs"]);

    // Claude Code takes the seconds a harness takes to start.
    providers.hold();
    state
        .command(
            cx,
            Command::ConfirmRelayAndSend {
                session_id: "mid-turn-relay".into(),
                text: "now review it".into(),
                attachment_paths: Vec::new(),
            },
        )
        .unwrap();
    cx.run_until_seen(|view| {
        view.timeline("mid-turn-relay")
            .entries
            .iter()
            .any(|entry| matches!(entry.content, EntryContent::ProviderRelay { .. }))
    });
    assert_eq!(queued(cx), ["now review it", "check the logs"]);
    let timeline = cx.seen().timeline("mid-turn-relay");
    assert_eq!(timeline.turns[0].status, Some(TurnStatus::Interrupted));
    let relay = timeline
        .entries
        .iter()
        .find(|entry| matches!(entry.content, EntryContent::ProviderRelay { .. }))
        .unwrap();
    assert_eq!(relay.turn, 1);
    let EntryContent::ProviderRelay { prompt, .. } = &relay.content else {
        unreachable!();
    };
    let prompt = prompt.clone().unwrap();
    assert!(prompt.contains("They differ in"));
    assert!(prompt.contains("still running when the conversation was handed over"));
    // The timeline discloses the handoff exactly as it goes out.
    providers.let_go();
    let claude = providers.started(cx, 1);
    assert_eq!(claude.provider, ProviderKind::ClaudeCode);
    let (delivery_id, handoff) = claude.next_turn(cx);
    assert_eq!(handoff, prompt);
    // Only the first message carries the conversation.
    claude.play([
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: "claude-turn".into(),
        },
        completed_turn("claude-turn"),
    ]);
    assert_eq!(claude.next_turn(cx).1, "check the logs");
}

/// A fork through a turn that ran before a handoff is a fork of the session
/// that ran it, on the backend that holds it: Codex forks its own session at
/// the restore point of the prompt that followed, or all of it through its
/// last turn, and no handoff is made.
#[test]
fn a_fork_through_a_turn_before_a_handoff_is_of_the_session_that_ran_it() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-fork-before-handoff");
    let (state, providers) = handed_over(cx, &test_store);
    for (n, (turn, checkpoint, point)) in [
        (
            0,
            "codex-1",
            agent::ForkPoint::Before {
                checkpoint_id: "codex-2".into(),
            },
        ),
        (1, "codex-2", agent::ForkPoint::Tail),
    ]
    .into_iter()
    .enumerate()
    {
        let command = serde_json::from_value(serde_json::json!({
            "type": "fork_thread",
            "content": { "id": HANDED, "at": { "turn": turn, "checkpoint_id": checkpoint } },
        }))
        .unwrap();
        let Ok(CommandResponse::SessionId(Some(fork))) = state.command(cx, command) else {
            panic!("turn {turn} forks");
        };
        state.open(cx, &fork);
        cx.run_until_seen(|view| view.timeline(&fork).turns.len() == turn + 1);
        assert!(cx.seen().status(&fork).relay_confirmation.is_none());
        state.send(cx, &fork, "go on");
        let codex = providers.started(cx, 2 + n);
        assert_eq!(codex.provider, ProviderKind::Codex);
        assert_eq!(codex.options.model.as_deref(), Some("gpt-5"));
        assert_eq!(codex.options.resume, Some(native("codex-native")));
        assert_eq!(codex.options.fork, Some(point));
        assert!(codex.options.option_selections.contains(&codex_effort()));
        assert_eq!(codex.next_turn(cx).1, "go on");
    }
}

/// A rewind to a message that ran before a handoff takes the thread back to
/// the backend that ran it, where its harness makes the rewind in its own
/// session. A rewind that harness refuses leaves the thread where it was.
#[test]
fn a_rewind_to_a_message_before_a_handoff_is_made_by_the_harness_that_ran_it() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-rewind-before-handoff");
    let (state, providers) = handed_over(cx, &test_store);

    rewind_handed(&state, cx, 1);
    let codex = providers.started(cx, 2);
    assert_eq!(codex.provider, ProviderKind::Codex);
    assert_eq!(codex.options.resume, Some(native("codex-native")));
    assert_eq!(codex.options.fork, None);
    assert!(matches!(
        codex.next_command(cx),
        SessionCommand::Rewind { checkpoint_id, mode: RewindMode::Conversation }
            if checkpoint_id == "codex-2"
    ));
    codex.play([AgentEvent::RewindFailed {
        checkpoint_id: "codex-2".into(),
        mode: RewindMode::Conversation,
        failure: agent::RewindFailure::new(agent::RewindFailureReason::TargetNotFound, None),
    }]);
    cx.run_until_seen(|view| {
        view.timeline(HANDED)
            .entries
            .iter()
            .any(|entry| matches!(entry.content, EntryContent::RewindFailed { .. }))
    });
    assert!(has_relay(cx));
    cx.run_until_seen(|view| view.status(HANDED).provider == ProviderKind::ClaudeCode);
    assert_eq!(
        backend(cx),
        (ProviderKind::ClaudeCode, Some("claude-opus-5-5".into()))
    );
    // The thread goes on in the session Claude Code holds.
    state.send(cx, HANDED, "fourth");
    let claude = providers.started(cx, 3);
    assert_eq!(claude.provider, ProviderKind::ClaudeCode);
    assert_eq!(claude.options.resume, Some(native("claude-native")));
    claude.start_turn(cx, "claude-2-turn");
    claude.play([completed_turn("claude-2-turn")]);
    claude.fence(cx, HANDED);

    rewind_handed(&state, cx, 1);
    let codex = providers.started(cx, 4);
    assert_eq!(codex.options.resume, Some(native("codex-native")));
    assert!(codex.options.option_selections.contains(&codex_effort()));
    codex.next_command(cx);
    codex.play([AgentEvent::RewindCompleted {
        checkpoint_id: "codex-2".into(),
        mode: RewindMode::Conversation,
        prefill: Some("second".into()),
        prefill_attachments: Vec::new(),
        later_history: agent::LaterHistory::Truncated,
        files: None,
        incomplete: None,
        undo_token: None,
    }]);
    cx.run_until_seen(|view| {
        view.timeline(HANDED)
            .entries
            .iter()
            .any(|entry| matches!(entry.content, EntryContent::Rewound(_)))
    });
    assert!(!has_relay(cx));
    cx.run_until_seen(|view| view.status(HANDED).provider == ProviderKind::Codex);
    assert_eq!(backend(cx), (ProviderKind::Codex, Some("gpt-5".into())));
    assert!(cx.seen().status(HANDED).relay_confirmation.is_none());
}

/// A rewind to the message a handoff carried undoes the handoff: the thread
/// goes on in the session it left, as it stands, with no harness asked to
/// rewind anything, and the next message is that session's, not a handoff.
#[test]
fn a_rewind_to_the_message_a_handoff_carried_returns_to_the_session_it_left() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-rewind-handoff");
    let (state, providers) = handed_over(cx, &test_store);

    rewind_handed(&state, cx, 2);
    cx.run_until_seen(|view| {
        view.timeline(HANDED)
            .entries
            .iter()
            .any(|entry| matches!(entry.content, EntryContent::Rewound(_)))
    });
    assert!(!has_relay(cx));
    cx.run_until_seen(|view| view.status(HANDED).provider == ProviderKind::Codex);
    assert_eq!(backend(cx), (ProviderKind::Codex, Some("gpt-5".into())));
    // The log says the handoff was undone, not that a harness rewound.
    let records = cx.seen().records(HANDED);
    assert!(
        records
            .iter()
            .any(|record| matches!(record.event, AgentEvent::RelayUndone { .. }))
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.event, AgentEvent::RewindCompleted { .. }))
    );

    // The first provider started since is the one the message goes to: none
    // was started to rewind.
    state.send(cx, HANDED, "third again");
    let codex = providers.started(cx, 2);
    assert_eq!(codex.provider, ProviderKind::Codex);
    assert_eq!(codex.options.resume, Some(native("codex-native")));
    assert_eq!(codex.options.fork, None);
    assert!(codex.options.option_selections.contains(&codex_effort()));
    assert_eq!(codex.next_turn(cx).1, "third again");
}

/// A thread moved from a quick chat into a new project keeps its transcript, survives a
/// restart of the host in its new place, and its next turn starts a fresh provider session
/// in the new directory that is handed the conversation so far. A busy thread and a thread
/// with its own worktree are refused with the reason.
#[test]
fn moved_thread_continues_in_the_new_project_with_a_handoff() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-move-session-test");
    let quick = test_store.root().join("quick");
    let demo = test_store.root().join("demo");
    std::fs::create_dir_all(&quick).unwrap();
    std::fs::create_dir_all(demo.join("app")).unwrap();
    let quick_project = Project::from_root(quick.clone());
    test_store.upsert_project(&quick_project).unwrap();
    // A thread of the same project that works in a worktree of its own.
    stored_thread(
        &test_store,
        "worktree-thread",
        ProviderKind::ClaudeCode,
        &quick,
        |worktree| {
            worktree.project_id = Some(quick_project.id.clone());
            worktree.worktree = Some(WorktreeInfo {
                root_project_path: quick.clone(),
                base: "main".into(),
                branch: "tcode/worktree-thread".into(),
            });
        },
    );
    let (launcher, launches) = recording_launcher();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher(launcher.clone());
        state
    });

    let Ok(CommandResponse::SessionId(Some(session_id))) = acked(
        &state,
        cx,
        1,
        Command::StartDraft {
            project_id: quick_project.id.clone(),
            cwd: quick.clone(),
        },
    ) else {
        panic!("draft not started");
    };
    state.subscribe(cx, Topic::Index);
    state.open(cx, &session_id);
    acked(
        &state,
        cx,
        2,
        Command::SendTurn {
            session_id: session_id.clone(),
            text: "let's start the demo".into(),
            attachment_paths: Vec::new(),
        },
    )
    .unwrap();
    let (delivery_id, _, events) = next_turn(cx, &launches, 0);
    for event in [
        AgentEvent::SessionStarted {
            provider_session_id: "native-1".into(),
            resume: agent::ResumeCursor(serde_json::json!("native-1")),
            model: None,
        },
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        AgentEvent::ItemCompleted(ThreadItem {
            id: "assistant-1".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "The demo will be a static site.".into(),
                summarized: false,
            },
        }),
    ] {
        events.try_send(event).unwrap();
    }
    cx.run_until_seen(|view| view.status(&session_id).running_turn.is_some());

    let Ok(CommandResponse::ProjectId(Some(demo_id))) =
        acked(&state, cx, 3, Command::CreateProject { root: demo.clone() })
    else {
        panic!("project not created");
    };
    let move_to_demo = || Command::MoveSession {
        session_id: session_id.clone(),
        project_id: demo_id.clone(),
        cwd: Some(PathBuf::from("app")),
    };
    assert_eq!(
        acked(&state, cx, 4, move_to_demo()).unwrap_err().code,
        "thread_busy"
    );
    events
        .try_send(AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: TurnStatus::Completed,
            usage: None,
        })
        .unwrap();
    cx.run_until_seen(|view| view.status(&session_id).running_turn.is_none());
    let refusal = acked(
        &state,
        cx,
        5,
        Command::MoveSession {
            session_id: "worktree-thread".into(),
            project_id: demo_id.clone(),
            cwd: None,
        },
    )
    .unwrap_err();
    assert_eq!(refusal.code, "worktree_session");

    acked(&state, cx, 6, move_to_demo()).unwrap();
    let meta = cx.seen().session(&session_id).unwrap();
    assert_eq!(meta.project_id.as_deref(), Some(demo_id.as_str()));
    assert_eq!(meta.cwd, demo.join("app"));
    assert_eq!(meta.resume_cursor, None);
    cx.run_until_parked();
    drop(state);

    // A host restarted after the move keeps the thread where it went, with its transcript.
    // It is another process, which nothing the stopped host left running reaches.
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher(launcher);
        state
    });
    state.subscribe(cx, Topic::Index);
    let meta = cx.seen().session(&session_id).unwrap();
    assert_eq!(meta.project_id.as_deref(), Some(demo_id.as_str()));
    assert_eq!(meta.cwd, demo.join("app"));
    state.open(cx, &session_id);
    acked(
        &state,
        cx,
        7,
        Command::SendTurn {
            session_id: session_id.clone(),
            text: "what did we decide?".into(),
            attachment_paths: Vec::new(),
        },
    )
    .unwrap();
    let (_, text, _) = next_turn(cx, &launches, 1);
    {
        let launches = launches.lock().unwrap();
        assert_eq!(launches[1].0.cwd, demo.join("app"));
        assert_eq!(launches[1].0.resume, None, "a fresh provider session");
    }
    assert!(
        text.starts_with(tcode_core::relay::RELAY_PREAMBLE),
        "{text}"
    );
    assert!(text.contains("let's start the demo"), "{text}");
    assert!(text.contains("The demo will be a static site."), "{text}");
    assert!(
        text.contains(&format!(
            "moved from `{}` to `{}`",
            quick.display(),
            demo.join("app").display()
        )),
        "{text}"
    );
    assert!(
        text.ends_with("what did we decide?\n</new-user-message>"),
        "{text}"
    );
    cx.run_until_seen(|view| view.session(&session_id).unwrap().moved_from.is_none());
    let relay = cx
        .seen()
        .timeline(&session_id)
        .entries
        .iter()
        .find_map(|entry| match &entry.content {
            EntryContent::ProviderRelay {
                prompt, moved_from, ..
            } => Some((prompt.clone(), moved_from.clone())),
            _ => None,
        })
        .expect("the handoff is shown in the thread");
    assert_eq!(relay, (Some(text.clone()), Some(quick.clone())));
}

/// A moved thread's first message hands the provider the conversation in the host's
/// log whichever comes first, the provider up or the log read: the message waits for
/// the read, and the thread forgets where it moved from only once the handoff went out.
#[test]
fn a_moved_threads_handoff_waits_for_its_log() {
    let test_store = TestStore::new("tcode-handoff-waits-for-log");
    let quick = test_store.root().join("quick");
    let demo = test_store.root().join("demo");
    std::fs::create_dir_all(&quick).unwrap();
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::ClaudeCode,
        &demo,
        |meta| {
            meta.moved_from = Some(quick.clone());
        },
    );
    for (ts, event) in [
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        AgentEvent::ItemCompleted(ThreadItem {
            id: "assistant-1".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "The demo will be a static site.".into(),
                summarized: false,
            },
        }),
        completed_turn("turn-1"),
    ]
    .iter()
    .enumerate()
    {
        test_store.append_event("thread", ts as u64, event).unwrap();
    }
    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    assert_eq!(
        cx.seen().session("thread").unwrap().moved_from,
        Some(quick.clone())
    );
    // The client opens the thread and sends at once: the host reads its log
    // and starts its provider off the host, and either may land first.
    state.deliver(
        cx,
        1,
        tcode_protocol::ClientPayload::Subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionStatus {
                session_id: "thread".into(),
            },
            after: None,
        }),
    );
    state.dispatch_command(
        cx,
        2,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "what did we decide?".into(),
            attachment_paths: Vec::new(),
        },
    );
    let (_, text) = providers.started(cx, 0).next_turn(cx);
    assert!(
        text.starts_with(tcode_core::relay::RELAY_PREAMBLE),
        "{text}"
    );
    assert!(text.contains("The demo will be a static site."), "{text}");
    assert!(
        text.contains(&format!(
            "moved from `{}` to `{}`",
            quick.display(),
            demo.display()
        )),
        "{text}"
    );
    assert!(
        text.ends_with("what did we decide?\n</new-user-message>"),
        "{text}"
    );
    cx.run_until_seen(|view| view.session("thread").unwrap().moved_from.is_none());
}

/// A thread on a harness whose native session goes along to another directory keeps that
/// session through a move: moved while its provider is down, the provider resumes it in the
/// new directory; moved while its provider runs, that provider is sent there before the next
/// turn. When the harness moves its session itself, the thread follows it there. No turn is
/// a handoff, and the thread shows each move once.
#[test]
fn a_moved_thread_keeps_a_native_session_that_can_move() {
    let test_store = TestStore::new("tcode-native-move");
    let quick = test_store.root().join("quick");
    let demo = test_store.root().join("demo");
    std::fs::create_dir_all(&quick).unwrap();
    std::fs::create_dir_all(&demo).unwrap();
    let quick_project = Project::from_root(quick.clone());
    let demo_project = Project::from_root(demo.clone());
    test_store.upsert_project(&quick_project).unwrap();
    test_store.upsert_project(&demo_project).unwrap();
    let native = agent::ResumeCursor(serde_json::json!({"session_id": "native-1"}));
    let mut meta = SessionMeta::new(ProviderKind::OpenCode, quick.clone(), None);
    meta.id = "thread".into();
    meta.project_id = Some(quick_project.id.clone());
    meta.resume_cursor = Some(native.clone());
    test_store.upsert_meta(&meta).unwrap();
    for (ts, event) in [
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        AgentEvent::ItemCompleted(ThreadItem {
            id: "assistant-1".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "The demo will be a static site.".into(),
                summarized: false,
            },
        }),
        AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: TurnStatus::Completed,
            usage: None,
        },
    ]
    .iter()
    .enumerate()
    {
        test_store.append_event("thread", ts as u64, event).unwrap();
    }
    let cx = &mut TestAppContext::default();
    let (launcher, launches) = recording_launcher();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher(launcher);
        state
    });
    let move_to = |project: &Project| Command::MoveSession {
        session_id: "thread".into(),
        project_id: project.id.clone(),
        cwd: None,
    };
    let send = |text: &str| Command::SendTurn {
        session_id: "thread".into(),
        text: text.into(),
        attachment_paths: Vec::new(),
    };

    state.subscribe(cx, Topic::Index);
    acked(&state, cx, 1, move_to(&demo_project)).unwrap();
    assert_eq!(
        cx.seen().session("thread").unwrap().resume_cursor,
        Some(native.clone())
    );
    state.open(cx, "thread");
    acked(&state, cx, 2, send("what did we decide?")).unwrap();
    let (delivery_id, text, events) = next_turn(cx, &launches, 0);
    {
        let launches = launches.lock().unwrap();
        assert_eq!(launches[0].0.cwd, demo);
        assert_eq!(launches[0].0.resume, Some(native.clone()));
    }
    assert_eq!(text, "what did we decide?");
    for event in [
        AgentEvent::SessionStarted {
            provider_session_id: "native-1".into(),
            resume: native.clone(),
            model: None,
        },
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: "turn-2".into(),
        },
        AgentEvent::TurnCompleted {
            turn_id: "turn-2".into(),
            status: TurnStatus::Completed,
            usage: None,
        },
    ] {
        events.try_send(event).unwrap();
    }
    cx.run_until_seen(|view| {
        let status = view.status("thread");
        view.timeline("thread").turns.len() == 2
            && status.running_turn.is_none()
            && status.delivery_in_flight.is_none()
    });

    acked(&state, cx, 3, move_to(&quick_project)).unwrap();
    acked(&state, cx, 4, send("and now?")).unwrap();
    cx.run_until(|_| !launches.lock().unwrap()[0].1.is_empty());
    let moved = next_command(&launches.lock().unwrap()[0].1);
    assert!(
        matches!(&moved, Ok(SessionCommand::MoveTo { cwd }) if *cwd == quick),
        "{moved:?}"
    );
    let (delivery_id, text, events) = next_turn(cx, &launches, 0);
    assert_eq!(text, "and now?");

    // The harness reports the move it was sent, then one its model makes during the turn.
    let elsewhere = test_store.root().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    for event in [
        AgentEvent::SessionMoved {
            from: demo.clone(),
            to: quick.clone(),
            parent_item_id: None,
        },
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: "turn-3".into(),
        },
        AgentEvent::SessionMoved {
            from: quick.clone(),
            to: elsewhere.clone(),
            parent_item_id: None,
        },
        AgentEvent::TurnCompleted {
            turn_id: "turn-3".into(),
            status: TurnStatus::Completed,
            usage: None,
        },
    ] {
        events.try_send(event).unwrap();
    }
    cx.run_until_seen(|view| {
        let status = view.status("thread");
        view.timeline("thread").turns.len() == 3
            && status.running_turn.is_none()
            && status.delivery_in_flight.is_none()
    });
    acked(&state, cx, 5, send("go on")).unwrap();
    let (_, text, _) = next_turn(cx, &launches, 0);
    assert_eq!(text, "go on");
    assert_eq!(
        launches.lock().unwrap().len(),
        1,
        "the same provider process"
    );
    {
        let meta = cx.seen().session("thread").unwrap();
        assert_eq!(meta.cwd, elsewhere);
        assert_eq!(meta.moved_from, None);
        let moves: Vec<_> = cx
            .seen()
            .timeline("thread")
            .entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::SessionMoved { from, to } => Some((from.clone(), to.clone())),
                EntryContent::ProviderRelay { .. } => panic!("a native move is no handoff"),
                _ => None,
            })
            .collect();
        assert_eq!(
            moves,
            [
                (quick.clone(), demo.clone()),
                (demo.clone(), quick.clone()),
                (quick.clone(), elsewhere.clone())
            ]
        );
    }
}

/// A worktree made for a native subagent is where that subagent's mirror works, and the
/// thread that asked owns it: deleting the thread removes it.
#[test]
fn a_subagents_claude_worktree_belongs_to_the_thread_and_goes_with_it() {
    use agent::{WorktreeAnswer, WorktreeRequest};
    let test_store = TestStore::new("tcode-claude-subagent-worktree");
    let repo = test_store.root().join("repo");
    init_git_repo(&repo);
    let project = Project::from_root(repo.clone());
    test_store.upsert_project(&project).unwrap();
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, repo.clone(), None);
    meta.id = "thread".into();
    meta.project_id = Some(project.id.clone());
    test_store.upsert_meta(&meta).unwrap();
    let cx = &mut TestAppContext::default();
    let (launcher, launches) = recording_launcher();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher(launcher);
        state
    });
    state.subscribe(cx, Topic::Index);
    state.open(cx, "thread");
    acked(
        &state,
        cx,
        1,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "isolate a subagent".into(),
            attachment_paths: Vec::new(),
        },
    )
    .unwrap();
    let (delivery_id, _, events) = next_turn(cx, &launches, 0);
    for event in [
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        AgentEvent::ItemStarted(ThreadItem {
            id: "toolu_agent".into(),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: "general-purpose".into(),
                description: "Print working directory".into(),
                status: ItemStatus::InProgress,
                summary: None,
                model: None,
                effort: None,
            },
        }),
        AgentEvent::WorktreeRequested {
            request_id: "hook-1".into(),
            request: WorktreeRequest::Create {
                cwd: repo.clone(),
                name: "agent-a955ec08a6e37c75e".into(),
            },
        },
    ] {
        events.try_send(event).unwrap();
    }
    let (_, answer) = next_worktree_answer(cx, &launches, 0);
    let WorktreeAnswer::Ready { path: worktree, .. } = answer else {
        panic!("{answer:?}")
    };
    events
        .try_send(AgentEvent::SessionMoved {
            from: repo.clone(),
            to: worktree.clone(),
            parent_item_id: Some("toolu_agent".into()),
        })
        .unwrap();
    cx.run_until_seen(|view| {
        view.sessions()
            .any(|meta| meta.native_subagent.is_some() && meta.cwd == worktree)
    });
    let thread = cx.seen().session("thread").unwrap();
    assert_eq!(thread.cwd, repo, "the session stays where it was");
    assert_eq!(thread.other_worktrees.len(), 1);

    state.dispatch_command(
        cx,
        2,
        Command::DeleteSession {
            session_id: "thread".into(),
            remove_worktree: true,
        },
    );
    cx.run_until(|_| !worktree.exists());
}

/// Switching a Claude Code thread to another account takes its native session along: the
/// turn running when the switch is made finishes under the old account, and the next one
/// restarts the provider under the new account, which resumes the same session from a copy
/// in its own home, with no relay and nothing asked. Only that session is copied. Switching
/// back carries the newer copy home again. A backend that is not an account still asks for
/// the relay.
#[test]
fn switching_accounts_carries_the_native_session() {
    let cx = &mut TestAppContext::default();
    let switch = AccountSwitch::new(cx, "tcode-account-transfer", true);
    assert_eq!(
        switch.launched(0),
        (Some(switch.default_home.clone()), None)
    );

    switch.send(cx, 10, "draft the notes").unwrap();
    let (delivery_id, _, events) = next_turn(cx, &switch.launches, 0);
    switch.select(cx, 11, Some("proxy")).unwrap();
    assert_eq!(
        switch.send(cx, 12, "ship it").unwrap_err().code,
        "relay_confirmation_required"
    );
    switch.select(cx, 13, Some("work")).unwrap();
    switch.run_turn(cx, &events, delivery_id, "Notes drafted.");
    // The usage read after that turn is the new account's, which the process still
    // running the old one cannot answer.
    assert!(matches!(
        switch.launches.lock().unwrap()[0].1.try_recv(),
        Err(smol::channel::TryRecvError::Empty)
    ));
    switch.send(cx, 14, "ship it").unwrap();
    let (delivery_id, text, events) = next_turn(cx, &switch.launches, 1);
    assert_eq!(text, "ship it");
    assert!(matches!(
        next_command(&switch.launches.lock().unwrap()[0].1),
        Ok(SessionCommand::Shutdown)
    ));
    assert_eq!(
        switch.launched(1),
        (Some(switch.work_home.clone()), native_session())
    );
    let read = |home: &Path, path: &str| std::fs::read_to_string(home.join(path)).ok();
    let project = format!("projects/{CLI_PROJECT_DIR}");
    for path in [
        format!("{project}/native-1.jsonl"),
        format!("{project}/native-1/subagents/agent-a.jsonl"),
        format!("{project}/native-1/subagents/agent-a.meta.json"),
        format!("{project}/native-1/tool-results/toolu_1.txt"),
        "file-history/native-1/9c1e@v1".into(),
        "tasks/native-1/1.json".into(),
        "session-env/native-1/sessionstart-hook-0.sh".into(),
        "todos/native-1-agent-native-1.json".into(),
        "todos/native-1-agent-a.json".into(),
        "plans/quiet-release-plan.md".into(),
        "plans/quiet-release-plan-agent-a.md".into(),
    ] {
        assert!(read(&switch.work_home, &path).is_some(), "{path}");
        assert_eq!(
            read(&switch.work_home, &path),
            read(&switch.default_home, &path)
        );
    }
    for path in [
        format!("{project}/other-session.jsonl"),
        "tasks/other-session/1.json".into(),
        "todos/other-session-agent-other-session.json".into(),
        "plans/other-plan.md".into(),
    ] {
        assert_eq!(read(&switch.work_home, &path), None, "{path}");
    }
    std::fs::write(
        switch.work_home.join(format!("{project}/native-1.jsonl")),
        "{\"turn\":1}\n{\"turn\":2}\n",
    )
    .unwrap();
    switch.run_turn(cx, &events, delivery_id, "Tagged.");

    switch.select(cx, 15, None).unwrap();
    switch.send(cx, 16, "and the changelog").unwrap();
    let (_, text, _) = next_turn(cx, &switch.launches, 2);
    assert_eq!(text, "and the changelog");
    assert_eq!(
        switch.launched(2),
        (Some(switch.default_home.clone()), native_session())
    );
    assert_eq!(
        read(&switch.default_home, &format!("{project}/native-1.jsonl")).as_deref(),
        Some("{\"turn\":1}\n{\"turn\":2}\n")
    );
    assert!(switch.relays().is_empty());
}

/// When the session the thread switched accounts with is not where the old account keeps
/// it, the new account starts a fresh session that the switching message hands the
/// conversation to, as a relay does.
#[test]
fn an_account_switch_relays_when_the_session_cannot_be_copied() {
    let cx = &mut TestAppContext::default();
    let switch = AccountSwitch::new(cx, "tcode-account-transfer-missing", false);

    switch.select(cx, 10, Some("work")).unwrap();
    switch.send(cx, 11, "ship it").unwrap();
    let (_, text, _) = next_turn(cx, &switch.launches, 1);
    assert_eq!(switch.launched(1), (Some(switch.work_home.clone()), None));
    assert!(
        text.starts_with(tcode_core::relay::RELAY_PREAMBLE),
        "{text}"
    );
    assert!(text.contains("Tag v1 on Friday."), "{text}");
    assert!(text.ends_with("ship it\n</new-user-message>"), "{text}");
    assert_eq!(switch.relays(), [Some(text)]);
}

/// When the new account's provider closes before it resumes the copied session, the next
/// message starts a fresh session there and hands it the conversation, the failed turn
/// included.
#[test]
fn an_account_switch_relays_when_the_copy_does_not_resume() {
    let cx = &mut TestAppContext::default();
    let switch = AccountSwitch::new(cx, "tcode-account-transfer-unresumed", true);

    switch.select(cx, 10, Some("work")).unwrap();
    switch.send(cx, 11, "ship it").unwrap();
    let (delivery_id, _, events) = next_turn(cx, &switch.launches, 1);
    assert_eq!(
        switch.launched(1),
        (Some(switch.work_home.clone()), native_session())
    );
    for event in [
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::SessionClosed {
            reason: Some("No conversation found with session ID: native-1".into()),
        },
    ] {
        events.try_send(event).unwrap();
    }
    let session_id = switch.session_id.clone();
    cx.run_until(|state| {
        state
            .resident(&session_id)
            .is_some_and(|session| matches!(session.runtime, Runtime::Idle))
    });

    switch.send(cx, 12, "try again").unwrap();
    let (_, text, _) = next_turn(cx, &switch.launches, 2);
    assert_eq!(switch.launched(2), (Some(switch.work_home.clone()), None));
    assert!(
        text.starts_with(tcode_core::relay::RELAY_PREAMBLE),
        "{text}"
    );
    assert!(text.contains("ship it"), "{text}");
    assert!(text.ends_with("try again\n</new-user-message>"), "{text}");
    assert_eq!(switch.relays(), [Some(text)]);
}

/// One finished turn of a harness with no rewind of its own, as its adapter reports it: the
/// restore point is Tcode's own, and what the turn reported writing is in the thread's
/// journal.
fn relayed_turn(
    cx: &mut TestAppContext,
    launches: &Launches,
    launch: usize,
    (turn, checkpoint, answer): (&str, &str, &str),
    write: Option<(&Path, &Path, &[u8])>,
) -> String {
    let (delivery_id, text, events) = next_turn(cx, launches, launch);
    if let Some((journal, file, content)) = write {
        // As the adapter journals the whole-file diff the agent reports.
        let before = std::fs::read(file).ok();
        std::fs::write(file, content).unwrap();
        agent::write_journal::WriteJournal::open(journal.to_path_buf())
            .record_write(checkpoint, file, before.as_deref(), content)
            .unwrap();
    }
    for event in [
        AgentEvent::SessionStarted {
            provider_session_id: format!("native-{launch}"),
            resume: agent::ResumeCursor(serde_json::json!({ "session_id": launch })),
            model: None,
        },
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: turn.into(),
        },
        AgentEvent::TurnCheckpoint {
            turn_id: turn.into(),
            checkpoint_id: checkpoint.into(),
        },
        AgentEvent::ItemCompleted(ThreadItem {
            id: format!("answer-{checkpoint}"),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: answer.into(),
                summarized: false,
            },
        }),
        AgentEvent::TurnCompleted {
            turn_id: turn.into(),
            status: TurnStatus::Completed,
            usage: None,
        },
    ] {
        events.try_send(event).unwrap();
    }
    text
}

/// Antigravity, maintained natively though it has only what standard ACP carries, is asked
/// for no rewind and no fork: Tcode makes them. A rewind puts back the files the thread's
/// sessions reported writing from the turn on and cuts the log; the next message starts a session anew and hands it
/// what the cut left, which the thread shows. The turns before stay targets of a later
/// rewind, and a fork is a copy of the log that its first message hands to a session of
/// its own.
#[test]
fn a_harness_with_no_rewind_or_fork_is_rewound_and_forked_by_tcode() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-relayed-history");
    let cwd = test_store.root().join("project");
    let note = cwd.join("note.txt");
    let session_id = "agy-thread".to_string();
    stored_thread(
        &test_store,
        &session_id,
        ProviderKind::Antigravity,
        &cwd,
        |_| {},
    );
    std::fs::write(&note, b"mine\r\n").unwrap();
    let (launcher, launches) = recording_launcher();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher(launcher);
        state
    });
    state.subscribe(cx, Topic::Index);
    state.open(cx, &session_id);
    let send = |cx: &mut TestAppContext, id: u64, thread: &str, text: &str| {
        acked(
            &state,
            cx,
            id,
            Command::SendTurn {
                session_id: thread.into(),
                text: text.into(),
                attachment_paths: Vec::new(),
            },
        )
        .unwrap();
    };
    let answered = |cx: &mut TestAppContext, thread: &str, checkpoint: &str| {
        let answer = format!("answer-{checkpoint}");
        cx.run_until_seen(|view| {
            view.timeline(thread)
                .entries
                .iter()
                .any(|entry| entry.id == answer)
                && view.status(thread).running_turn.is_none()
        });
    };
    // The cut takes the turn's answer out of the thread and leaves its divider.
    let rewound = |cx: &mut TestAppContext, checkpoint: &str| {
        let answer = format!("answer-{checkpoint}");
        cx.run_until_seen(|view| {
            let entries = view.timeline(&session_id).entries;
            entries
                .iter()
                .any(|entry| matches!(entry.content, EntryContent::Rewound(_)))
                && !entries.iter().any(|entry| entry.id == answer)
        });
    };
    let rewind = |cx: &mut TestAppContext, id: u64, turn: usize, mode: RewindMode| {
        acked(
            &state,
            cx,
            id,
            Command::RewindTurn {
                session_id: session_id.clone(),
                turn,
                steer: None,
                checkpoint_id: None,
                mode,
                client_id: None,
            },
        )
    };

    send(cx, 1, &session_id, "write the note");
    let journal = {
        let launches = launches.lock().unwrap();
        launches[0].0.write_journal.clone().unwrap()
    };
    assert!(
        journal.starts_with(test_store.root()) && !journal.starts_with(&cwd),
        "the journal is in Tcode's data, not the workspace: {}",
        journal.display()
    );
    relayed_turn(
        cx,
        &launches,
        0,
        ("turn-1", "restore-1", "The note says one."),
        Some((&journal, &note, b"one\n")),
    );
    answered(cx, &session_id, "restore-1");
    send(cx, 2, &session_id, "now change it");
    relayed_turn(
        cx,
        &launches,
        0,
        ("turn-2", "restore-2", "The note says two."),
        Some((&journal, &note, b"two\n")),
    );
    answered(cx, &session_id, "restore-2");

    // The fork is of the whole thread as it stands, made before anything is rewound.
    let Ok(CommandResponse::SessionId(Some(fork_id))) = acked(
        &state,
        cx,
        3,
        Command::ForkThread {
            id: session_id.clone(),
            at: None,
        },
    ) else {
        panic!("the thread was not forked");
    };

    rewind(cx, 4, 1, RewindMode::FilesAndConversation).unwrap();
    rewound(cx, "restore-2");
    assert_eq!(std::fs::read(&note).unwrap(), b"one\n");
    {
        let launches = launches.lock().unwrap();
        let mut asked = Vec::new();
        while let Ok(command) = next_command(&launches[0].1) {
            asked.push(command);
        }
        assert!(
            matches!(asked.as_slice(), [SessionCommand::Shutdown]),
            "the harness is asked for no rewind, and its session is left: {asked:?}"
        );
    }
    let meta = cx.seen().session(&session_id).unwrap();
    assert_eq!(meta.resume_cursor, None);

    send(cx, 5, &session_id, "say it another way");
    let text = relayed_turn(
        cx,
        &launches,
        1,
        ("turn-1", "restore-3", "The note still says one."),
        None,
    );
    assert_eq!(
        launches.lock().unwrap()[1].0.resume,
        None,
        "a session started anew"
    );
    assert!(
        text.starts_with(tcode_core::relay::RELAY_PREAMBLE),
        "{text}"
    );
    assert!(text.contains("The note says one."), "{text}");
    assert!(!text.contains("The note says two."), "{text}");
    assert!(
        text.ends_with("say it another way\n</new-user-message>"),
        "{text}"
    );
    answered(cx, &session_id, "restore-3");
    let handed = cx
        .seen()
        .timeline(&session_id)
        .entries
        .iter()
        .find_map(|entry| match &entry.content {
            EntryContent::ProviderRelay { prompt, .. } => prompt.clone(),
            _ => None,
        });
    assert_eq!(handed, Some(text), "what the session was handed is shown");

    // The first turn ran in a session the thread has since left twice over.
    rewind(cx, 6, 0, RewindMode::FilesAndConversation).unwrap();
    rewound(cx, "restore-1");
    assert_eq!(std::fs::read(&note).unwrap(), b"mine\r\n");

    let fork = cx.seen().session(&fork_id).unwrap();
    assert_eq!((&fork.resume_cursor, &fork.pending_fork), (&None, &None));
    state.open(cx, &fork_id);
    send(cx, 7, &fork_id, "go on from here");
    let (_, text, _) = next_turn(cx, &launches, 2);
    {
        let launches = launches.lock().unwrap();
        assert_eq!((&launches[2].0.resume, &launches[2].0.fork), (&None, &None));
    }
    assert!(text.contains("The note says two."), "{text}");
    assert!(
        text.ends_with("go on from here\n</new-user-message>"),
        "{text}"
    );
}
