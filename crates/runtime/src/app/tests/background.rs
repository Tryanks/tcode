use super::*;

/// A restart would kill a background task, so it waits for the task; a message
/// does not. It goes out on the running process, says it kept the old settings,
/// and the message after the work ends takes the restart.
#[test]
fn settings_restart_waits_for_background_follow_up() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-background-restart-test");
    let (state, providers, actor) = settings_scene(
        cx,
        &test_store,
        "background-restart",
        1,
        "claude-sonnet-4-6",
    );
    state.send(cx, "background-restart", "goes out beside the task");
    assert!(matches!(
        actor.next_command(cx),
        SessionCommand::ApplySettings { .. }
    ));
    actor.play([AgentEvent::SettingsApplied { applied: false }]);
    let (delivery_id, text) = actor.next_turn(cx);
    assert_eq!(text, "goes out beside the task");
    actor.play([AgentEvent::TurnAccepted { delivery_id }]);
    actor.fence(cx, "background-restart");
    let entries = cx.seen().timeline("background-restart").entries;
    assert!(
        matches!(entries.last().unwrap().content, EntryContent::SettingsKept),
        "the message says it kept the old settings: {:?}",
        entries
            .iter()
            .map(|entry| &entry.content)
            .collect::<Vec<_>>()
    );
    actor.play([
        AgentEvent::TurnStarted {
            turn_id: "beside-the-task".into(),
        },
        completed_turn("beside-the-task"),
    ]);
    actor.fence(cx, "background-restart");
    assert!(
        next_command(&actor.commands).is_err(),
        "the running task keeps its process"
    );

    // An early drain still cannot restart the provider: its self-invoked
    // follow-up turn may be about to start.
    actor.play([AgentEvent::BackgroundTasksChanged { count: 0 }]);
    actor.fence(cx, "background-restart");
    assert!(next_command(&actor.commands).is_err());

    providers.hold();
    state.send(cx, "background-restart", "takes the new model");
    assert!(matches!(actor.next_command(cx), SessionCommand::Shutdown));
    assert_eq!(
        cx.seen()
            .status("background-restart")
            .queued_messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        ["takes the new model"]
    );
    providers.let_go();
}

/// A harness that can take a new model and effort where it runs is asked to,
/// and the message that asked goes out behind its answer, on the same process,
/// background work or none.
#[test]
fn a_new_model_is_taken_on_the_running_process() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-live-settings-test");
    let (state, _providers, actor) =
        settings_scene(cx, &test_store, "live-settings", 1, "claude-sonnet-4-6");
    let effort = OptionSelection {
        id: "reasoningEffort".into(),
        value: serde_json::json!("low"),
    };
    state
        .command(
            cx,
            Command::SetActiveOption {
                session_id: "live-settings".into(),
                id: effort.id.clone(),
                value: Some(effort.value.clone()),
            },
        )
        .unwrap();
    state.send(cx, "live-settings", "on the new model");
    match actor.next_command(cx) {
        SessionCommand::ApplySettings {
            model,
            option_selections,
        } => {
            assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
            assert!(option_selections.contains(&effort), "{option_selections:?}");
        }
        other => panic!("expected the settings to go first: {other:?}"),
    }
    assert!(
        next_command(&actor.commands).is_err(),
        "the message waits its answer"
    );
    actor.play([AgentEvent::SettingsApplied { applied: true }]);
    let (delivery_id, _) = actor.next_turn(cx);
    actor.play([AgentEvent::TurnAccepted { delivery_id }]);
    actor.fence(cx, "live-settings");
    assert!(
        !cx.seen()
            .timeline("live-settings")
            .entries
            .iter()
            .any(|entry| matches!(entry.content, EntryContent::SettingsKept)),
        "the message ran on the settings it asked for"
    );
    let status = cx.seen().status("live-settings");
    assert!(!status.model_pending_restart && !status.options_pending_restart);
}

/// What the process refuses takes a restart at once when nothing would be
/// killed. With background work running, the message goes out on the old
/// settings instead (settings_restart_waits_for_background_follow_up).
#[test]
fn a_refused_settings_change_falls_back_to_the_restart() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-refused-settings-test");
    let (state, _providers, actor) =
        settings_scene(cx, &test_store, "refused-settings", 0, "claude-sonnet-4-6");
    state.send(cx, "refused-settings", "go");
    assert!(matches!(
        actor.next_command(cx),
        SessionCommand::ApplySettings { .. }
    ));
    actor.play([AgentEvent::SettingsApplied { applied: false }]);
    assert!(matches!(actor.next_command(cx), SessionCommand::Shutdown));
}

/// The list a harness publishes reaches the client's status and a stop reaches
/// the harness, which the status shows until the harness drops it; one that
/// lists nothing is refused, not asked, and so is a move into the background
/// where the harness has none. A call whose work the harness has
/// dropped from its list but not yet ended reads as ended, and one going on
/// in the background that the harness never lists is listed after its own.
#[test]
fn background_tasks_are_listed_and_stopped_where_the_harness_can() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-background-stop-test");
    stored_thread(
        &test_store,
        "lists",
        ProviderKind::ClaudeCode,
        &test_store.root().join("lists"),
        |_| {},
    );
    stored_thread(
        &test_store,
        "cannot",
        ProviderKind::Pi,
        &test_store.root().join("cannot"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let task = agent::BackgroundTask {
        id: "bl5ti808a".into(),
        name: "sleep 300".into(),
        kind: agent::BackgroundKind::Command,
        item_id: Some("toolu-sleep".into()),
        started_at: 1,
        started_reported: false,
    };
    let listed = |cx: &TestAppContext| cx.seen().status("lists").background_tasks.clone();
    let actor = first_turn(&state, cx, &providers, "lists", 0);
    let started = |id: &str, content| {
        AgentEvent::ItemStarted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        })
    };
    let backgrounded = |id: &str| AgentEvent::ItemBackgrounded {
        item_id: id.into(),
        parent_item_id: None,
    };
    actor.play([
        started(
            "toolu-sleep",
            ItemContent::CommandExecution {
                command: "sleep 300".into(),
                input: Vec::new(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::InProgress,
            },
        ),
        backgrounded("toolu-sleep"),
        started(
            "toolu-review",
            ItemContent::Subagent {
                agent_type: "general".into(),
                description: "Review the migration".into(),
                status: ItemStatus::InProgress,
                summary: None,
                model: None,
                effort: None,
            },
        ),
        backgrounded("toolu-review"),
        // The subagent's own work opens its mirror's turn.
        AgentEvent::ItemStarted(ThreadItem {
            id: "toolu-review-read".into(),
            parent_item_id: Some("toolu-review".into()),
            content: ItemContent::CommandExecution {
                command: "cat migration.sql".into(),
                input: Vec::new(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::InProgress,
            },
        }),
        AgentEvent::BackgroundTaskList {
            tasks: vec![task.clone()],
        },
    ]);
    actor.fence(cx, "lists");
    // The subagent runs in the background, and counts there alone.
    assert_eq!(
        cx.seen().activity("lists").unwrap().background,
        tcode_protocol::BackgroundActivity {
            tasks: 2,
            agents: 0
        }
    );
    let unlisted = |listed: &[BackgroundTaskStatus]| {
        listed
            .iter()
            .find(|status| status.task.id == "toolu-review")
            .map(|status| {
                (
                    status.listed,
                    status.ended,
                    status.task.kind,
                    status.task.name.clone(),
                )
            })
    };
    let now = listed(cx);
    assert_eq!(
        (
            now.len(),
            &now[0].task,
            now[0].listed,
            now[0].stopping_since
        ),
        (2, &task, true, None)
    );
    assert_eq!(
        unlisted(&now),
        Some((
            false,
            false,
            agent::BackgroundKind::Agent,
            "Review the migration".into()
        ))
    );
    let pi = first_turn(&state, cx, &providers, "cannot", 1);

    for (session_id, task_id) in [
        ("lists", Some(task.id.clone())),
        ("lists", None),
        ("cannot", None),
    ] {
        let _ = state.command(
            cx,
            Command::StopBackgroundTask {
                session_id: session_id.into(),
                task_id,
            },
        );
    }
    for session_id in ["lists", "cannot"] {
        let _ = state.command(
            cx,
            Command::MoveToBackground {
                session_id: session_id.into(),
                item_id: "toolu-build".into(),
            },
        );
    }
    assert!(matches!(
        actor.next_command(cx),
        SessionCommand::StopBackgroundTask { task_id: Some(id) } if id == task.id
    ));
    assert!(matches!(
        actor.next_command(cx),
        SessionCommand::StopBackgroundTask { task_id: None }
    ));
    assert!(matches!(
        actor.next_command(cx),
        SessionCommand::MoveToBackground { item_id } if item_id == "toolu-build"
    ));
    assert!(next_command(&actor.commands).is_err());
    assert!(next_command(&pi.commands).is_err(), "Pi is never asked");
    assert!(
        listed(cx)[0].stopping_since.is_some(),
        "the stop shows until the harness drops the task"
    );

    actor.play([AgentEvent::BackgroundTaskList { tasks: Vec::new() }]);
    actor.fence(cx, "lists");
    let now = listed(cx);
    let sleep = now
        .iter()
        .find(|status| status.task.id == "toolu-sleep")
        .expect("the call is still open");
    assert!(
        sleep.ended && !sleep.listed && sleep.stopping_since.is_none(),
        "{sleep:?}"
    );
    assert_eq!(unlisted(&now).map(|(_, ended, ..)| ended), Some(false));
}

/// A harness that no longer answers a stop can be killed: its turn ends as
/// interrupted and the thread stops stopping.
#[test]
fn force_terminate_kills_a_harness_that_does_not_answer_a_stop() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-force-terminate-test");
    stored_thread(
        &test_store,
        "unanswering",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let actor = first_turn(&state, cx, &providers, "unanswering", 0);
    state
        .command(
            cx,
            Command::Interrupt {
                session_id: "unanswering".into(),
            },
        )
        .unwrap();
    assert!(matches!(actor.next_command(cx), SessionCommand::Interrupt));
    let status = cx.seen().status("unanswering");
    assert!(status.stopping && status.stopping_since.is_some());

    state
        .command(
            cx,
            Command::ForceTerminate {
                session_id: "unanswering".into(),
            },
        )
        .unwrap();
    assert!(matches!(actor.next_command(cx), SessionCommand::Shutdown));
    cx.run_until_seen(|view| has_record(view, "unanswering", is_turn_end));
    assert_eq!(
        cx.seen()
            .timeline("unanswering")
            .turns
            .last()
            .unwrap()
            .status,
        Some(TurnStatus::Interrupted)
    );
    let status = cx.seen().status("unanswering");
    assert!(!status.stopping && !status.activity.turn_running);
}

/// A provider still starting has no turn to interrupt: Stop leaves the start
/// alone (the harness may already hold the conversation), hands the message it
/// waited for back to whoever pressed Stop, and the thread reads as stopping;
/// a start that never ends is ended by Force quit. The provider is an ACP
/// agent that reads its stdin and never answers `initialize` (cline 3.0.39
/// does the same).
#[test]
fn a_stop_while_the_provider_starts_hands_the_message_back_and_force_quit_ends_the_start() {
    const ROLE: &str = "TCODE_TEST_SILENT_AGENT";
    const NAME: &str = "app::tests::a_stop_while_the_provider_starts_hands_the_message_back_and_force_quit_ends_the_start";
    if std::env::var_os(ROLE).is_some() {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        return;
    }
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-stop-while-starting-test");
    let cwd = test_store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let project = Project::from_root(cwd.clone());
    test_store.upsert_project(&project).unwrap();
    let (ended_tx, ended) = smol::channel::unbounded();
    let launcher = ProviderLauncher(Arc::new(move |_, mut options: SessionOptions| {
        options.acp = Some(agent::AcpAgent {
            id: "silent".into(),
            name: "Silent".into(),
            launch: agent::AcpLaunch::Binary {
                command: std::env::current_exe().unwrap(),
                args: vec!["--exact".into(), NAME.into(), "--quiet".into()],
                env: vec![(ROLE.into(), "1".into())],
            },
        });
        let ended = ended_tx.clone();
        Box::pin(async move {
            let started = agent::start_session(ProviderKind::Acp, options).await;
            let _ = ended.send(started.is_err()).await;
            started
        })
    }));
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher(launcher);
        state
    });
    let Ok(CommandResponse::SessionId(Some(session_id))) = state.command(
        cx,
        Command::StartDraft {
            project_id: project.id.clone(),
            cwd,
        },
    ) else {
        panic!("the host starts a draft")
    };
    state.open(cx, &session_id);
    state.dispatch_command(
        cx,
        2,
        Command::SendTurn {
            session_id: session_id.clone(),
            text: "start".into(),
            attachment_paths: Vec::new(),
        },
    );
    cx.run_until_seen(|view| view.status(&session_id).preparing_turn);

    let stopped = through_pipe(
        &state,
        cx,
        3,
        tcode_protocol::ClientPayload::Command(Command::Interrupt {
            session_id: session_id.clone(),
        }),
    );
    assert!(stopped.iter().any(|message| matches!(
        message,
        HostMessage::Ack { id: 3, result: Ok(CommandResponse::Unsent(unsent)) }
            if unsent.len() == 1 && unsent[0].text == "start"
    )));
    assert!(ended.try_recv().is_err(), "the start goes on");
    let status = cx.seen().status(&session_id);
    assert!(status.stopping);
    assert!(status.queued_messages.is_empty());

    state.dispatch_command(
        cx,
        4,
        Command::ForceTerminate {
            session_id: session_id.clone(),
        },
    );
    assert_eq!(ended.recv_blocking(), Ok(true));
    cx.run_until_seen(|view| {
        let status = view.status(&session_id);
        !status.preparing_turn && !status.stopping && !status.activity.working
    });
}

/// Background work takes no interrupt: Claude Code acknowledges one and the
/// task runs on, so a thread with nothing else running must not read as stopping.
#[test]
fn an_interrupt_leaves_a_thread_with_only_background_work_alone() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-background-interrupt-test");
    stored_thread(
        &test_store,
        "only-background",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let actor = first_turn(&state, cx, &providers, "only-background", 0);
    actor.play([
        AgentEvent::BackgroundTasksChanged { count: 1 },
        completed_turn("turn"),
    ]);
    cx.run_until_seen(|view| view.status("only-background").running_turn.is_none());
    let _ = state.command(
        cx,
        Command::Interrupt {
            session_id: "only-background".into(),
        },
    );
    assert!(next_command(&actor.commands).is_err());
    assert!(!cx.seen().status("only-background").stopping);
}

/// A report arriving while a restart waits on background work, with no turn to
/// steer into, wakes the lead on its running process.
#[test]
fn a_report_reaches_a_lead_whose_restart_waits_for_background_work() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-background-report-test");
    let fleet = Fleet::new(
        cx,
        &test_store,
        ProviderKind::ClaudeCode,
        &["child"],
        |meta| {
            meta.model = Some("claude-opus-4-8".into());
        },
    );
    fleet.lead.play([
        AgentEvent::BackgroundTasksChanged { count: 1 },
        completed_turn("turn"),
    ]);
    cx.run_until_seen(|view| view.status("parent").running_turn.is_none());
    fleet
        .state
        .command(
            cx,
            Command::SetActiveModel {
                session_id: "parent".into(),
                provider: ProviderKind::ClaudeCode,
                model: Some("claude-sonnet-4-6".into()),
                profile_id: None,
            },
        )
        .unwrap();
    fleet.child("child").play([
        report_result_event("rep-1", "the report"),
        completed_turn("turn-1"),
    ]);
    assert!(matches!(
        fleet.lead.next_command(cx),
        SessionCommand::ApplySettings { .. }
    ));
    fleet
        .lead
        .play([AgentEvent::SettingsApplied { applied: false }]);
    assert!(fleet.lead.next_turn(cx).1.contains("the report"));
}

/// A lead's queue does not outlive the host: a report waiting there when the
/// host stops goes to the lead when it starts again, and only until the lead's
/// provider takes it.
#[test]
fn a_report_queued_when_the_host_stops_reaches_the_lead_once_after_it_starts() {
    let test_store = TestStore::new("tcode-report-restart-test");
    // The lead's turn runs on a harness that cannot steer, so the report waits.
    let cx = &mut TestAppContext::default();
    let fleet = Fleet::new(cx, &test_store, ProviderKind::Cursor, &["child"], |_| {});
    fleet.child("child").play([AgentEvent::SessionClosed {
        reason: Some("the provider closed".into()),
    }]);
    cx.run_until_seen(|view| {
        view.status("parent")
            .queued_messages
            .iter()
            .any(|message| message.text.contains("the provider closed"))
    });
    assert!(next_command(&fleet.lead.commands).is_err());
    fleet
        .state
        .command(cx, Command::ShutdownAllAndFlush)
        .unwrap();

    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &test_store);
    let lead = providers.started(cx, 0);
    let (delivery_id, text) = lead.next_turn(cx);
    assert!(text.contains("the provider closed"), "{text}");
    lead.play([AgentEvent::TurnAccepted { delivery_id }]);
    state.open(cx, "parent");
    lead.fence(cx, "parent");
    state.command(cx, Command::ShutdownAllAndFlush).unwrap();

    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &test_store);
    state.open(cx, "parent");
    let status = cx.seen().status("parent");
    assert!(
        status.queued_messages.is_empty() && !status.preparing_turn,
        "taken once, sent once"
    );
    assert_eq!(providers.count(), 0);
}

/// Work a thread left running in the background ends as interrupted when
/// the thread is reopened after the host restarted, since no process can
/// finish it; a thread whose process still runs keeps it open, and so does
/// a native subagent's mirror whose parent's process does.
#[test]
fn background_work_no_process_can_finish_ends_as_interrupted_on_load() {
    fn backgrounded(parent: Option<&str>) -> Vec<AgentEvent> {
        vec![
            AgentEvent::ItemStarted(ThreadItem {
                id: "serve".into(),
                parent_item_id: parent.map(Into::into),
                content: ItemContent::CommandExecution {
                    command: "serve".into(),
                    input: Vec::new(),
                    output: String::new(),
                    exit_code: None,
                    status: ItemStatus::InProgress,
                },
            }),
            AgentEvent::ItemBackgrounded {
                item_id: "serve".into(),
                parent_item_id: parent.map(Into::into),
            },
        ]
    }
    fn status(view: &ClientView, id: &str) -> ItemStatus {
        view.timeline(id)
            .entries
            .iter()
            .find_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::CommandExecution { status, .. }) => Some(*status),
                _ => None,
            })
            .unwrap()
    }
    let test_store = TestStore::new("tcode-unowned-background-test");
    for id in ["cold", "live"] {
        stored_thread(
            &test_store,
            id,
            ProviderKind::ClaudeCode,
            &test_store.root().join(id),
            |_| {},
        );
    }
    {
        // A host that stopped while the cold thread's work ran.
        let cx = &mut TestAppContext::default();
        let (state, providers) = played_host(cx, &test_store);
        let cold = first_turn(&state, cx, &providers, "cold", 0);
        cold.play(backgrounded(None));
        cold.play([completed_turn("turn")]);
        cx.run_until_seen(|view| has_record(view, "cold", is_turn_end));
    }

    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let live = first_turn(&state, cx, &providers, "live", 0);
    live.play([AgentEvent::ItemStarted(subagent_item(
        "task",
        None,
        "general",
        "Serve the docs",
        ItemStatus::InProgress,
    ))]);
    live.play(backgrounded(Some("task")));
    live.play(backgrounded(None));
    live.play([completed_turn("turn")]);
    cx.run_until_seen(|view| has_record(view, "live", is_turn_end));
    let mirror = cx.seen().mirror("task").unwrap().id.clone();
    state.leave(cx, "live");

    state.open(cx, "cold");
    cx.run_until_seen(|view| status(view, "cold") == ItemStatus::Interrupted);
    assert!(matches!(
        &cx.seen().records("cold").last().unwrap().event,
        AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::CommandExecution {
                status: ItemStatus::Interrupted,
                ..
            },
            ..
        })
    ));
    state.open(cx, "live");
    state.open(cx, &mirror);
    live.fence(cx, "live");
    assert_eq!(status(cx.seen(), "live"), ItemStatus::InProgress);
    assert_eq!(status(cx.seen(), &mirror), ItemStatus::InProgress);
}

/// A provider that refuses a steer because the turn just ended has not seen it:
/// a child's report steered into that turn goes out as a turn of its own.
#[test]
fn a_rejected_steer_of_a_report_goes_out_as_a_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-rejected-report-test");
    let fleet = Fleet::new(cx, &test_store, ProviderKind::Codex, &["child"], |_| {});
    fleet.child("child").play([
        report_result_event("rep-1", "the report"),
        completed_turn("turn-1"),
    ]);
    let SessionCommand::Steer {
        request_id, text, ..
    } = fleet.lead.next_command(cx)
    else {
        panic!("a running turn takes the report as a steer");
    };
    fleet.lead.play([
        completed_turn("turn"),
        AgentEvent::SteerRejected { request_id },
    ]);
    assert_eq!(fleet.lead.next_turn(cx).1, text);
}

/// A steer the provider never confirmed died with its process, so it goes back
/// on the queue, still the agent's.
#[test]
fn a_steer_lost_with_its_process_goes_back_on_the_queue() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-lost-steer-test");
    let fleet = Fleet::new(
        cx,
        &test_store,
        ProviderKind::ClaudeCode,
        &["child"],
        |_| {},
    );
    fleet.child("child").play([
        report_result_event("rep-1", "the report"),
        completed_turn("turn-1"),
    ]);
    let report = fleet.lead.next_steer(cx);
    fleet.lead.play([AgentEvent::SessionClosed {
        reason: Some("claude exited".into()),
    }]);
    cx.run_until_seen(|view| {
        has_record(view, "parent", |event| {
            matches!(event, AgentEvent::SessionClosed { .. })
        })
    });
    let queued = &cx.seen().status("parent").queued_messages;
    assert_eq!(
        queued
            .iter()
            .map(|message| &message.text)
            .collect::<Vec<_>>(),
        [&report]
    );
    // It goes out as the agent's when the lead runs again.
    fleet.state.leave(cx, "parent");
    fleet.state.open(cx, "parent");
    let resumed = fleet.providers.started(cx, 2);
    let (delivery_id, text) = resumed.next_turn(cx);
    assert_eq!(text, report);
    resumed.play([AgentEvent::TurnAccepted { delivery_id }]);
    cx.run_until_seen(|view| {
        view.records("parent").iter().any(|record| {
            record.origin == Some(MessageOrigin::Agent)
                && matches!(
                    &record.event,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::UserMessage { text, .. },
                        ..
                    }) if *text == report
                )
        })
    });
}

/// A message sent with a new model while a turn runs (a `/compact` included)
/// waits for that turn instead of killing it to restart the provider.
#[test]
fn settings_restart_waits_for_the_running_turn() {
    use tcode_core::settings::EnvVar;
    for launch_environment in [false, true] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-running-turn-restart-test");
        let mut settings = Settings::default();
        settings.profiles.insert(
            "endpoint".into(),
            ProviderProfile {
                kind: ProviderKind::Grok,
                settings: ProviderSettings {
                    env: vec![EnvVar {
                        name: "BASE_URL".into(),
                        value: "https://old.invalid".into(),
                        sensitive: false,
                    }],
                    ..Default::default()
                },
            },
        );
        SettingsStore::new(store.root().clone())
            .save(&settings)
            .unwrap();
        stored_thread(
            &store,
            "thread",
            ProviderKind::Grok,
            &store.root().join("thread"),
            |meta| {
                meta.model = Some("grok-4".into());
                if launch_environment {
                    meta.profile_id = Some("endpoint".into());
                }
            },
        );
        let (state, providers) = played_host(cx, &store);
        let old = first_turn(&state, cx, &providers, "thread", 0);
        if launch_environment {
            assert!(
                old.options
                    .launch_env
                    .env
                    .contains(&("BASE_URL".into(), "https://old.invalid".into()))
            );
        }
        state
            .command(
                cx,
                if launch_environment {
                    Command::UpdateProfileSettings {
                        profile_id: "endpoint".into(),
                        patch: ProfileSettingsPatch::SetEnv(vec![EnvVar {
                            name: "BASE_URL".into(),
                            value: "https://new.invalid".into(),
                            sensitive: false,
                        }]),
                    }
                } else {
                    Command::SetActiveModel {
                        session_id: "thread".into(),
                        provider: ProviderKind::Grok,
                        model: Some("grok-5".into()),
                        profile_id: None,
                    }
                },
            )
            .unwrap();
        state.send(cx, "thread", "continue after configuration change");
        assert!(next_command(&old.commands).is_err());
        providers.hold();
        old.play([completed_turn("turn")]);
        assert!(matches!(old.next_command(cx), SessionCommand::Shutdown));
        let current = providers.started(cx, 1);
        assert_eq!(cx.seen().status("thread").queued_messages.len(), 1);
        if launch_environment {
            assert!(
                current
                    .options
                    .launch_env
                    .env
                    .contains(&("BASE_URL".into(), "https://new.invalid".into()))
            );
            assert!(
                !current
                    .options
                    .launch_env
                    .env
                    .contains(&("BASE_URL".into(), "https://old.invalid".into()))
            );
        } else {
            assert_eq!(current.options.model.as_deref(), Some("grok-5"));
        }
        providers.let_go();
        assert_eq!(
            current.next_turn(cx).1,
            "continue after configuration change"
        );
        assert!(next_command(&old.commands).is_err());
    }
}
