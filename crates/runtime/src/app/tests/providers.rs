use super::*;

/// A harness that describes its own controls takes back the ones it no
/// longer lists: they leave the thread's controls, what it runs at, and what
/// the user asked for.
#[test]
fn provider_config_options_remove_retired_wire_controls() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-provider-config-retirement");
    stored_thread(
        &store,
        "config-session",
        ProviderKind::Grok,
        &store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &store);
    let provider = first_turn(&state, cx, &providers, "config-session", 0);
    provider.play([AgentEvent::ProviderOptions {
        descriptors: vec![OptionDescriptor::Boolean {
            id: "thinking".into(),
            label: "Thinking".into(),
            default_value: true,
            role: Default::default(),
            apply: Default::default(),
            recommended: None,
            permissive: None,
        }],
        selections: vec![OptionSelection {
            id: "thinking".into(),
            value: serde_json::json!(true),
        }],
    }]);
    cx.run_until_seen(|view| {
        view.status("config-session")
            .provider_option_descriptors
            .iter()
            .any(|descriptor| matches!(descriptor, OptionDescriptor::Boolean { id, .. } if id == "thinking"))
    });
    state
        .command(
            cx,
            Command::SetActiveOption {
                session_id: "config-session".into(),
                id: "thinking".into(),
                value: Some(serde_json::json!(false)),
            },
        )
        .unwrap();
    let offered = cx.seen().status("config-session").clone();
    let thinking = |selections: &[OptionSelection]| {
        selections
            .iter()
            .any(|selection| selection.id == "thinking")
    };
    assert!(thinking(&offered.provider_option_selections));
    assert!(thinking(&offered.provider_option_requested_selections));
    provider.play([AgentEvent::ProviderOptions {
        descriptors: Vec::new(),
        selections: Vec::new(),
    }]);
    provider.fence(cx, "config-session");
    let removed = cx.seen().status("config-session");
    assert!(
        !removed
            .provider_option_descriptors
            .iter()
            .any(|descriptor| {
                matches!(descriptor, OptionDescriptor::Boolean { id, .. } if id == "thinking")
            })
    );
    assert!(!thinking(&removed.provider_option_selections));
    assert!(!thinking(&removed.provider_option_requested_selections));
}

/// A permission the user picks is asked of the running harness, or waits for
/// its restart where the harness takes one only at launch, and shows as
/// asked for, not as in force, until the harness says it is.
#[test]
fn native_permission_selection_stays_pending_until_provider_confirmation() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-permission-confirmation");
    let cases = [
        (
            ProviderKind::ClaudeCode,
            "permissionMode",
            serde_json::json!("default"),
            serde_json::json!("auto"),
            false,
        ),
        (
            ProviderKind::Codex,
            "permissions",
            serde_json::json!("ask"),
            serde_json::json!("full_access"),
            false,
        ),
        (
            ProviderKind::OpenCode,
            "permission_mode",
            serde_json::json!("normal"),
            serde_json::json!("auto"),
            false,
        ),
        (
            ProviderKind::Grok,
            "permissionMode",
            serde_json::json!("default"),
            serde_json::json!("auto"),
            true,
        ),
    ];
    for (provider, ..) in &cases {
        let id = format!("permission-{provider:?}");
        stored_thread(&store, &id, *provider, &store.root().join(&id), |_| {});
    }
    let (state, providers) = played_host(cx, &store);
    for (n, (provider, id, initial, requested, restart)) in cases.into_iter().enumerate() {
        let session_id = format!("permission-{provider:?}");
        let session_id = session_id.as_str();
        let played = first_turn(&state, cx, &providers, session_id, n);
        let confirmed = |value| AgentEvent::ProviderOptions {
            descriptors: vec![permission_control(provider).unwrap()],
            selections: vec![OptionSelection {
                id: id.into(),
                value,
            }],
        };
        let value_of = |selections: &[OptionSelection]| {
            selections
                .iter()
                .find(|selection| selection.id == id)
                .map(|selection| selection.value.clone())
        };
        played.play([confirmed(initial.clone())]);
        played.fence(cx, session_id);
        // What a launch asks of the harness for the thread's permission went
        // before it said what it runs at.
        while let Ok(command) = next_command(&played.commands) {
            assert!(
                matches!(command, SessionCommand::SetOption { .. }),
                "{command:?}"
            );
        }
        assert!(!cx.seen().status(session_id).options_pending_restart);
        state
            .command(
                cx,
                Command::SetActiveOption {
                    session_id: session_id.into(),
                    id: id.into(),
                    value: Some(requested.clone()),
                },
            )
            .unwrap();
        if restart {
            assert!(next_command(&played.commands).is_err());
        } else {
            assert!(matches!(
                played.next_command(cx),
                SessionCommand::SetOption { id: sent_id, value } if sent_id == id && value == requested
            ));
        }
        played.play([confirmed(initial.clone())]);
        played.fence(cx, session_id);
        let pending = cx.seen().status(session_id);
        assert_eq!(
            value_of(&pending.provider_option_selections),
            Some(initial.clone())
        );
        assert_eq!(
            value_of(&pending.provider_option_requested_selections),
            Some(requested.clone())
        );
        assert_eq!(pending.options_pending_restart, restart);
        if !restart {
            played.play([confirmed(requested.clone())]);
            played.fence(cx, session_id);
            let applied = cx.seen().status(session_id);
            assert_eq!(
                value_of(&applied.provider_option_selections),
                Some(requested.clone())
            );
            assert_eq!(
                applied.provider_option_requested_selections,
                applied.provider_option_selections
            );
            assert!(!applied.options_pending_restart);
        }
        assert!(next_command(&played.commands).is_err());
    }
}

/// A message sent after the user chose another context window, while the
/// harness still runs at the old one, says so as it goes out.
#[test]
fn queued_message_stamps_live_context_window_change() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-context-window-stamp");
    stored_thread(
        &store,
        "thread",
        ProviderKind::ClaudeCode,
        &store.root().join("thread"),
        |meta| {
            meta.model = Some("claude-opus-5".into());
            meta.option_selections.push(OptionSelection {
                id: agent::context_window::OPTION_ID.into(),
                value: serde_json::json!("1m"),
            });
        },
    );
    let (state, providers) = played_host(cx, &store);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    state
        .command(
            cx,
            Command::SetActiveOption {
                session_id: "thread".into(),
                id: agent::context_window::OPTION_ID.into(),
                value: Some(serde_json::json!(500_000)),
            },
        )
        .unwrap();
    state.send(cx, "thread", "queued");
    provider.play([completed_turn("turn")]);
    // Claude Code takes the new window where it runs.
    assert!(matches!(
        provider.next_command(cx),
        SessionCommand::ApplySettings { .. }
    ));
    provider.play([AgentEvent::SettingsApplied { applied: true }]);
    let (delivery_id, text) = provider.next_turn(cx);
    assert_eq!(text, "queued");
    provider.play([AgentEvent::TurnAccepted { delivery_id }]);
    cx.run_until_seen(|view| {
        has_record(view, "thread", |event| {
            matches!(event, AgentEvent::ContextWindowChanged { window: 500_000 })
        })
    });
}

/// Stopping everything stops every harness running, shown or not.
#[test]
fn shutdown_all_notifies_active_and_parked_live_providers() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-app-test");
    let threads = [
        ("viewed", ProviderKind::Codex),
        ("parked", ProviderKind::ClaudeCode),
        ("other", ProviderKind::OpenCode),
    ];
    for (id, provider) in threads {
        stored_thread(
            &test_store,
            id,
            provider,
            &test_store.root().join(id),
            |_| {},
        );
    }
    let (state, providers) = played_host(cx, &test_store);
    let mut played = Vec::new();
    for (n, (id, _)) in threads.into_iter().enumerate() {
        let provider = first_turn(&state, cx, &providers, id, n);
        provider.play([completed_turn("turn")]);
        cx.run_until_seen(|view| view.status(id).running_turn.is_none());
        if id != "viewed" {
            state.leave(cx, id);
        }
        played.push(provider);
    }
    state.command(cx, Command::ShutdownAllAndFlush).unwrap();
    for provider in &played {
        assert!(matches!(
            provider.next_command(cx),
            SessionCommand::Shutdown
        ));
    }
}

/// A provider start the user gave up on, landing before or after the start
/// that replaced it, is shut down and delivers nothing; the current start
/// takes what waited for it, once.
#[test]
fn startup_generation_rejects_stale_same_session_attempt() {
    for stale_after_live in [false, true] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-startup-generation");
        stored_thread(
            &store,
            "same-session",
            ProviderKind::Codex,
            &store.root().join("thread"),
            |_| {},
        );
        let (state, providers) = played_host(cx, &store);
        providers.hold();
        state.open(cx, "same-session");
        state.send(cx, "same-session", "first message");
        let stale = providers.started(cx, 0);
        // The user gives up on the start, and sends again.
        state
            .command(
                cx,
                Command::ForceTerminate {
                    session_id: "same-session".into(),
                },
            )
            .unwrap();
        state.send(cx, "same-session", "second message");
        let current = providers.started(cx, 1);
        if stale_after_live {
            providers.let_go_of(1);
            assert_eq!(current.next_turn(cx).1, "first message");
            providers.let_go_of(0);
            assert!(matches!(stale.next_command(cx), SessionCommand::Shutdown));
        } else {
            providers.let_go_of(0);
            assert!(matches!(stale.next_command(cx), SessionCommand::Shutdown));
            providers.let_go_of(1);
            assert_eq!(current.next_turn(cx).1, "first message");
        }
        let delivery_id = cx.seen().status("same-session").delivery_in_flight.unwrap();
        current.play([AgentEvent::TurnAccepted { delivery_id }]);
        current.fence(cx, "same-session");
        assert!(next_command(&stale.commands).is_err());
        assert!(
            next_command(&current.commands).is_err(),
            "stale completion must not restart or duplicate current delivery"
        );
    }
}

/// A message the harness had not taken when its process closed waits, and
/// goes once to the process that resumes the thread; taking it twice records
/// it once.
#[test]
fn unaccepted_send_survives_eof_and_is_delivered_once_after_resume() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-acked-delivery-data");
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    // The preceding model turn has completed, but Claude still owns a
    // background process. This is the idle-send window from the repro.
    let first = first_turn(&state, cx, &providers, "thread", 0);
    first.play([
        AgentEvent::BackgroundTasksChanged { count: 1 },
        completed_turn("turn"),
    ]);
    cx.run_until_seen(|view| view.status("thread").running_turn.is_none());
    state.send(cx, "thread", "survive the eof race");
    let (delivery_id, submitted) = first.next_turn(cx);
    let delivered = |cx: &TestAppContext| {
        cx.seen()
            .records("thread")
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::UserMessage { text, .. },
                        ..
                    }) if text == "survive the eof race"
                )
            })
            .count()
    };
    assert_eq!(delivered(cx), 0);

    // EOF wins before the first actor writes, so no TurnAccepted exists.
    first.play([AgentEvent::SessionClosed {
        reason: Some("claude closed stdout".into()),
    }]);
    cx.run_until_seen(|view| {
        has_record(view, "thread", |event| {
            matches!(event, AgentEvent::SessionClosed { .. })
        })
    });
    // Coming back to the thread resumes it.
    state.leave(cx, "thread");
    state.open(cx, "thread");
    let resumed = providers.started(cx, 1);
    let (retried, text) = resumed.next_turn(cx);
    assert_eq!((retried, text), (delivery_id, submitted));
    // A duplicate acceptance cannot remove or persist anything twice.
    resumed.play([
        AgentEvent::TurnAccepted {
            delivery_id: retried,
        },
        AgentEvent::TurnAccepted {
            delivery_id: retried,
        },
    ]);
    resumed.fence(cx, "thread");
    assert_eq!(delivered(cx), 1);
}

/// The model a harness says it started on becomes the thread's, and the
/// next message goes to that process without restarting it for the "change".
#[test]
fn inferred_startup_model_updates_live_model_without_restart() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-live-model-sync-test");
    stored_thread(
        &test_store,
        "model-sync",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.open(cx, "model-sync");
    state.send(cx, "model-sync", "first message");
    let provider = providers.started(cx, 0);
    provider.play([AgentEvent::SessionStarted {
        provider_session_id: "provider-session".into(),
        resume: agent::ResumeCursor(serde_json::json!({
            "session_id": "provider-session"
        })),
        model: Some("claude-sonnet-4-6".into()),
    }]);
    let (delivery_id, _) = provider.next_turn(cx);
    provider.play([
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: "turn".into(),
        },
        completed_turn("turn"),
    ]);
    cx.run_until_seen(|view| has_record(view, "model-sync", is_turn_end));
    assert_eq!(
        cx.seen().status("model-sync").requested_model.as_deref(),
        Some("claude-sonnet-4-6")
    );
    state.send(cx, "model-sync", "second message");
    assert_eq!(
        provider.next_turn(cx).1,
        "second message",
        "phantom restart sent Shutdown"
    );
}

/// Coming back to a thread whose harness waits idle takes the same process
/// up again, and is no activity of the thread's.
#[test]
fn select_session_readopts_idle_resident_without_changing_recency_or_shutdown() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-readopt-test");
    stored_thread(
        &test_store,
        "idle-resident",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let provider = first_turn(&state, cx, &providers, "idle-resident", 0);
    provider.play([completed_turn("turn")]);
    cx.run_until_seen(|view| view.status("idle-resident").running_turn.is_none());
    let updated_at = cx.seen().session("idle-resident").unwrap().updated_at;
    // The thread was last active a minute ago.
    cx.clock.advance(Duration::from_secs(60));
    state.leave(cx, "idle-resident");
    state.open(cx, "idle-resident");
    provider.fence(cx, "idle-resident");
    assert!(
        next_command(&provider.commands).is_err(),
        "re-adoption sent Shutdown"
    );
    assert_eq!(providers.count(), 1);
    assert_eq!(
        cx.seen().session("idle-resident").unwrap().updated_at,
        updated_at
    );
    assert_eq!(
        test_store
            .load_index()
            .unwrap()
            .into_iter()
            .find(|meta| meta.id == "idle-resident")
            .unwrap()
            .updated_at,
        updated_at
    );
}

/// A harness left idle in a thread nobody shows is stopped once the grace a
/// harness is kept for has passed since its turn ended, however the thread
/// was left and come back to meanwhile.
#[test]
fn idle_grace_runs_from_the_last_turn_end_whether_viewed_or_parked() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-grace-turn-end-test");
    stored_thread(
        &test_store,
        "idle-resident",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let provider = first_turn(&state, cx, &providers, "idle-resident", 0);
    provider.play([completed_turn("turn")]);
    cx.run_until_seen(|view| view.status("idle-resident").running_turn.is_none());
    // Leaving the thread a minute later does not start the grace again.
    cx.clock.advance(Duration::from_secs(60));
    state.leave(cx, "idle-resident");
    cx.clock
        .advance(RESIDENT_IDLE_GRACE - Duration::from_secs(60));
    assert!(matches!(
        provider.next_command(cx),
        SessionCommand::Shutdown
    ));
}

/// A harness left idle is stopped once the grace has passed since its last
/// turn ended, not its first.
#[test]
fn a_later_turn_end_starts_the_idle_grace_again() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-grace-second-turn-test");
    stored_thread(
        &test_store,
        "idle-resident",
        ProviderKind::ClaudeCode,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let provider = first_turn(&state, cx, &providers, "idle-resident", 0);
    provider.play([completed_turn("turn")]);
    cx.run_until_seen(|view| view.status("idle-resident").running_turn.is_none());
    cx.clock.advance(Duration::from_secs(60));
    state.send(cx, "idle-resident", "again");
    provider.start_turn(cx, "second");
    provider.play([completed_turn("second")]);
    cx.run_until_seen(|view| has_record(view, "idle-resident", |event| {
        matches!(event, AgentEvent::TurnCompleted { turn_id, .. } if turn_id == "second")
    }));
    state.leave(cx, "idle-resident");
    cx.clock.advance(RESIDENT_IDLE_GRACE);
    assert!(matches!(
        provider.next_command(cx),
        SessionCommand::Shutdown
    ));
}

/// The host keeps a bounded number of idle harnesses for threads nobody
/// shows: leaving one more stops the one left longest ago, and only that one.
#[test]
fn resident_idle_lru_evicts_only_oldest_provider() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-lru-test");
    let ids = (0..=MAX_IDLE_RESIDENTS)
        .map(|index| format!("resident-{index}"))
        .collect::<Vec<_>>();
    for id in &ids {
        stored_thread(
            &test_store,
            id,
            ProviderKind::ClaudeCode,
            &test_store.root().join(id),
            |_| {},
        );
    }
    let (state, providers) = played_host(cx, &test_store);
    let mut played = Vec::new();
    for (n, id) in ids.iter().enumerate() {
        let provider = first_turn(&state, cx, &providers, id, n);
        provider.play([completed_turn("turn")]);
        cx.run_until_seen(|view| view.status(id).running_turn.is_none());
        cx.clock.advance(Duration::from_secs(1));
        state.leave(cx, id);
        played.push(provider);
    }
    assert!(matches!(
        next_command(&played[0].commands),
        Ok(SessionCommand::Shutdown)
    ));
    for provider in &played[1..] {
        assert!(
            next_command(&provider.commands).is_err(),
            "non-LRU resident was shut down"
        );
    }
}

/// A new thread starts at what its project names, else at what its Harness
/// names, else at the permission Tcode recommends for that Harness.
#[test]
fn a_new_threads_permission_is_its_projects_then_its_harness_then_the_recommended() {
    let store = TestStore::new("tcode-permission-default");
    let plain = Project::from_root(store.root().join("plain"));
    let mut configured = Project::from_root(store.root().join("configured"));
    configured
        .permission_defaults
        .insert("claude".into(), "default".into());
    for project in [&plain, &configured] {
        fs::create_dir_all(&project.root).unwrap();
        store.upsert_project(project).unwrap();
    }
    let cx = &mut TestAppContext::default();
    let (state, _) = played_host(cx, &store);
    for (project, harness, expected) in [
        (&plain, None, "auto"),
        (&plain, Some("acceptEdits"), "acceptEdits"),
        (&configured, Some("acceptEdits"), "default"),
    ] {
        state
            .command(
                cx,
                Command::UpdateProfileSettings {
                    profile_id: "claude".into(),
                    patch: ProfileSettingsPatch::SetDefaultPermission(harness.map(Into::into)),
                },
            )
            .unwrap();
        let CommandResponse::SessionId(Some(id)) = state
            .command(
                cx,
                Command::StartDraft {
                    project_id: project.id.clone(),
                    cwd: project.root.join(expected),
                },
            )
            .unwrap()
        else {
            panic!("draft response")
        };
        state.open(cx, &id);
        let status = cx.seen().status(&id);
        assert_eq!(status.provider, ProviderKind::ClaudeCode);
        assert_eq!(
            status
                .provider_option_requested_selections
                .iter()
                .find(|selection| selection.id == "permissionMode")
                .map(|selection| selection.value.as_str()),
            Some(Some(expected))
        );
    }
}

/// While a harness runs, an option it reports runs at what it reported and
/// at nothing until it has, never at its default; one it does not report runs
/// at what Tcode sent, and says so. What it reports leaves the thread's own
/// choice as it was.
#[test]
fn a_running_harness_options_are_what_it_reported_or_what_was_sent() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-confirmed-options");
    let chosen = |id: &str, value: serde_json::Value| OptionSelection {
        id: id.into(),
        value,
    };
    let value = |status: &SessionStatus, id: &str| {
        status
            .provider_option_selections
            .iter()
            .find(|selection| selection.id == id)
            .map(|selection| selection.value.clone())
    };
    stored_thread(
        &store,
        "confirmed",
        ProviderKind::ClaudeCode,
        &store.root().join("thread"),
        |meta| {
            meta.option_selections = vec![
                chosen(agent::EFFORT_OPTION_ID, serde_json::json!("high")),
                chosen("thinking", serde_json::json!(true)),
            ];
        },
    );
    let (state, providers) = played_host(cx, &store);
    let provider = first_turn(&state, cx, &providers, "confirmed", 0);
    {
        let level = |value: &str| agent::SelectOption {
            value: value.into(),
            label: value.into(),
            description: None,
            unavailable: None,
            speed: None,
            summary: None,
            approval: None,
        };
        provider.play([AgentEvent::ProviderOptions {
            descriptors: vec![
                OptionDescriptor::Select {
                    id: agent::EFFORT_OPTION_ID.into(),
                    label: "Effort".into(),
                    options: vec![level("medium"), level("high")],
                    default_value: Some("medium".into()),
                    role: agent::OptionRole::Effort,
                    apply: Default::default(),
                    recommended: None,
                    permissive: None,
                    minimum: None,
                },
                OptionDescriptor::Boolean {
                    id: "thinking".into(),
                    label: "Thinking".into(),
                    default_value: false,
                    role: agent::OptionRole::Thinking,
                    apply: Default::default(),
                    recommended: None,
                    permissive: None,
                },
            ],
            selections: Vec::new(),
        }]);
        provider.fence(cx, "confirmed");
        let before = cx.seen().status("confirmed").clone();
        assert_eq!(value(&before, agent::EFFORT_OPTION_ID), None);
        assert_eq!(value(&before, "thinking"), Some(serde_json::json!(true)));
        assert_eq!(before.provider_option_unconfirmed, ["thinking"]);
        assert_eq!(before.running_model, None);

        provider.play([
            AgentEvent::ProviderOptions {
                descriptors: Vec::new(),
                selections: vec![chosen(agent::EFFORT_OPTION_ID, serde_json::json!("medium"))],
            },
            AgentEvent::SessionModel {
                model: "claude-test".into(),
            },
        ]);
        provider.fence(cx, "confirmed");
        let after = cx.seen().status("confirmed").clone();
        assert_eq!(
            value(&after, agent::EFFORT_OPTION_ID),
            Some(serde_json::json!("medium"))
        );
        assert_eq!(
            after.provider_option_requested_selections[0],
            chosen(agent::EFFORT_OPTION_ID, serde_json::json!("high"))
        );
        assert_eq!(after.running_model.as_deref(), Some("claude-test"));
    }
}

/// A `$skill` the provider also exposes as a slash command (Claude) goes out
/// as that command; a skill-only mention (Codex) and an unknown `$word` are
/// sent as typed.
#[test]
fn skill_mentions_use_the_providers_native_invocation() {
    let command = |name: &str, kind| ProviderCommand {
        name: name.into(),
        description: None,
        kind,
    };
    let claude = [
        command("review", ProviderCommandKind::Command),
        command("review", ProviderCommandKind::Skill),
    ];
    let codex = [command("review", ProviderCommandKind::Skill)];
    assert_eq!(
        native_skill_invocation("$review the diff".into(), &claude),
        "/review the diff"
    );
    assert_eq!(
        native_skill_invocation("$review".into(), &claude),
        "/review"
    );
    assert_eq!(
        native_skill_invocation("$review the diff".into(), &codex),
        "$review the diff"
    );
    for text in ["$HOME is set", "please $review", "$"] {
        assert_eq!(native_skill_invocation(text.into(), &claude), text);
    }
}
