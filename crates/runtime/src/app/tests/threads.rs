use super::*;

#[test]
fn archive_and_unarchive_apply_exact_timestamp_cascades() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-archive-cascade-test");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    for (id, parent) in [
        ("parent", None),
        ("child", Some("parent")),
        ("grandchild", Some("child")),
    ] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_string);
        store.upsert_meta(&meta).unwrap();
    }
    let state = cx.new_entity(TestClientState::new(store.clone()));

    state.update(cx, |state, cx| {
        state.archive_session("parent", cx);
        let archived_at = state
            .sessions
            .iter()
            .find(|meta| meta.id == "parent")
            .unwrap()
            .archived_at
            .unwrap();
        assert!(
            state
                .sessions
                .iter()
                .all(|meta| meta.archived_at == Some(archived_at))
        );

        let grandchild = state
            .sessions
            .iter_mut()
            .find(|meta| meta.id == "grandchild")
            .unwrap();
        grandchild.archived_at = Some(archived_at + 1);
        let grandchild = grandchild.clone();
        state.persist_meta(&grandchild, cx);

        state.unarchive_session("parent", cx);
        assert_eq!(
            state
                .sessions
                .iter()
                .find(|meta| meta.id == "parent")
                .unwrap()
                .archived_at,
            None
        );
        assert_eq!(
            state
                .sessions
                .iter()
                .find(|meta| meta.id == "child")
                .unwrap()
                .archived_at,
            None
        );
        assert_eq!(
            state
                .sessions
                .iter()
                .find(|meta| meta.id == "grandchild")
                .unwrap()
                .archived_at,
            Some(archived_at + 1)
        );
    });
    cx.run_until_parked();
    let persisted = store.load_index().unwrap();
    assert!(
        persisted
            .iter()
            .find(|meta| meta.id == "grandchild")
            .unwrap()
            .archived_at
            .is_some()
    );
    assert!(
        persisted
            .iter()
            .filter(|meta| meta.id != "grandchild")
            .all(|meta| meta.archived_at.is_none())
    );
}

/// Deleting a thread deletes every thread under it, archived or running, in
/// the one command: a descendant's harness is stopped, and a host started
/// again has none of them or their logs.
#[test]
fn deleting_a_thread_deletes_every_descendant_in_one_command() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-delete-cascade-test");
    let root = test_store.root().clone();
    for (id, parent) in [
        ("parent", None),
        ("child", Some("parent")),
        ("grandchild", Some("child")),
        ("other", None),
    ] {
        stored_thread(
            &test_store,
            id,
            ProviderKind::Codex,
            &root.join("project"),
            |meta| {
                meta.parent_session_id = parent.map(str::to_string);
                if id == "child" {
                    meta.archived_at = Some(1);
                }
            },
        );
        test_store
            .append_event(id, 1, &AgentEvent::Warning { message: id.into() })
            .unwrap();
    }
    let (state, providers) = played_host(cx, &test_store);
    // The grandchild's harness runs on in the background.
    let grandchild = first_turn(&state, cx, &providers, "grandchild", 0);
    grandchild.play([turn_completed("turn", TurnStatus::Completed)]);
    cx.run_until_seen(|view| has_record(view, "grandchild", is_turn_end));
    state.leave(cx, "grandchild");

    state
        .command(
            cx,
            Command::DeleteSession {
                session_id: "parent".into(),
                remove_worktree: false,
            },
        )
        .unwrap();
    assert!(matches!(
        grandchild.next_command(cx),
        SessionCommand::Shutdown
    ));
    state.update(cx, |state, _| state.close_store().unwrap());

    let cx = &mut TestAppContext::default();
    let reopened = SessionStore::open_at(root).unwrap();
    let (state, _providers) = played_host(cx, &reopened);
    state.subscribe(cx, Topic::Index);
    assert_eq!(
        cx.seen()
            .sessions()
            .map(|meta| meta.id.as_str())
            .collect::<Vec<_>>(),
        ["other"]
    );
    let Ok(QueryResponse::ArchivedSessions(archived)) =
        state.query(cx, tcode_protocol::Query::ArchivedSessions)
    else {
        panic!("the host lists its archived threads")
    };
    assert!(archived.sessions.is_empty());
    for id in ["parent", "child", "grandchild"] {
        assert!(reopened.read_event_log(id).unwrap().is_empty(), "{id}");
    }
    assert!(!reopened.read_event_log("other").unwrap().is_empty());
}

#[test]
fn title_regeneration_uses_stored_history_and_preserves_intervening_changes() {
    let cases = [
        (
            "generated",
            "Improve QR Sharing",
            TurnStatus::Completed,
            None,
            Some("Improve QR Sharing"),
        ),
        (
            "cached",
            "Improve QR Sharing",
            TurnStatus::Completed,
            None,
            Some("Improve QR Sharing"),
        ),
        (
            "renamed",
            "Improve QR Sharing",
            TurnStatus::Completed,
            None,
            Some("My title"),
        ),
        (
            "deleted",
            "Improve QR Sharing",
            TurnStatus::Completed,
            None,
            None,
        ),
        ("failed", "", TurnStatus::Failed, None, Some("Old title")),
        (
            "formatted",
            "  **Title: Fix sidebar rename.**  ",
            TurnStatus::Completed,
            None,
            Some("Fix sidebar rename"),
        ),
        (
            "chinese",
            "# 「标题：为对话生成简洁标题。」",
            TurnStatus::Completed,
            None,
            Some("为对话生成简洁标题"),
        ),
        (
            "empty",
            "  ` `  ",
            TurnStatus::Completed,
            None,
            Some("Old title"),
        ),
        (
            "bounded",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            TurnStatus::Completed,
            None,
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa…"),
        ),
        (
            "zero-output",
            "Provider diagnostic",
            TurnStatus::Completed,
            Some(0),
            Some("Old title"),
        ),
        (
            "positive-output",
            "A real title",
            TurnStatus::Completed,
            Some(4),
            Some("A real title"),
        ),
        (
            "failed-turn",
            "Provider diagnostic",
            TurnStatus::Failed,
            None,
            Some("Old title"),
        ),
    ];
    // Every case is a thread of its own, stored before the host starts so
    // none but the one whose latest message was just sent is resident when
    // its title is asked for.
    let store = TestStore::new("tcode-regenerate-title");
    let mut settings = Settings::default();
    settings.title_generation.profile_id = Some("titles".into());
    settings.profiles.insert(
        "titles".into(),
        ProviderProfile {
            kind: ProviderKind::Codex,
            settings: ProviderSettings {
                env: vec![tcode_core::settings::EnvVar {
                    name: "BASE_URL".into(),
                    value: "https://titles.invalid".into(),
                    sensitive: false,
                }],
                ..Default::default()
            },
        },
    );
    tcode_services::settings::SettingsStore::new(store.root().clone())
        .save(&settings)
        .unwrap();
    let image = store.root().join("qr.png");
    fs::write(&image, [1, 2, 3]).unwrap();
    let ids = cases.map(|_| {
        let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
        meta.title = "Old title".into();
        meta.updated_at = 123;
        store.upsert_meta(&meta).unwrap();
        for (item_id, content) in [
            (
                "user",
                ItemContent::UserMessage {
                    text: "Improve QR sharing".into(),
                    attachments: vec![image.to_string_lossy().into_owned()],
                },
            ),
            (
                "assistant",
                ItemContent::AssistantMessage {
                    text: "The QR link expires too soon".into(),
                    summarized: false,
                },
            ),
        ] {
            store
                .append_event(
                    &meta.id,
                    1,
                    &AgentEvent::ItemCompleted(ThreadItem {
                        id: item_id.into(),
                        parent_item_id: None,
                        content,
                    }),
                )
                .unwrap();
        }
        meta.id
    });
    let cx = &mut TestAppContext::default();
    let providers = Providers::default();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*store).clone());
        state.ai_title_generation_enabled = true;
        state.set_provider_launcher(providers.launcher());
        state
    });
    state.subscribe(cx, Topic::Index);
    cx.drain_outgoing();
    let account_thread = ids[0].clone();
    let mut launches = 0;
    for ((outcome, raw_title, status, output_tokens, expected), id) in cases.into_iter().zip(ids) {
        if outcome == "cached" {
            // The thread's latest message was taken the moment before, and
            // its provider runs it.
            state.open(cx, &id);
            state.send(cx, &id, "Include link expiration");
            providers.started(cx, launches).start_turn(cx, "turn");
            launches += 1;
            cx.run_until_seen(|view| view.status(&id).running_turn.is_some());
            cx.drain_outgoing();
        }
        let updated_at = cx.seen().session(&id).unwrap().updated_at;
        for request_id in [1, 2] {
            state.dispatch_command(
                cx,
                request_id,
                Command::RegenerateSessionTitle {
                    session_id: id.clone(),
                },
            );
        }
        let scripted = providers.started(cx, launches);
        launches += 1;
        if outcome == "generated" {
            assert!(
                scripted
                    .options
                    .launch_env
                    .env
                    .contains(&("BASE_URL".into(), "https://titles.invalid".into(),))
            );
        }
        let prompt = match cx.run_until_recv(&scripted.commands) {
            SessionCommand::SendTurn {
                text, attachments, ..
            } => {
                assert_eq!(attachments.len(), 1);
                assert_eq!(attachments[0].data_base64, "AQID");
                text
            }
            other => panic!("expected a title request, got {other:?}"),
        };
        cx.run_until_parked();
        assert!(
            scripted.commands.try_recv().is_err() && providers.count() == launches,
            "duplicate requests must coalesce"
        );
        assert!(prompt.contains("Old title"));
        let context: String =
            serde_json::from_str(prompt.split_once("Conversation JSON: ").unwrap().1).unwrap();
        let mut expected_context = "USER:\nImprove QR sharing\n[Attachments: qr.png]\n\nASSISTANT:\nThe QR link expires too soon".to_string();
        if outcome == "cached" {
            expected_context.push_str("\n\nUSER:\nInclude link expiration");
        }
        assert_eq!(context, expected_context);
        assert!(cx.drain_outgoing().iter().any(|message| matches!(
            message,
            HostMessage::Event(EventEnvelope { event: ServerEvent::IndexSummaryReplaced(summary), .. })
                if summary.title_generating.contains(&id)
        )), "pending state must reach other clients");

        match outcome {
            "renamed" => state.dispatch_command(
                cx,
                3,
                Command::RenameSession {
                    session_id: id.clone(),
                    title: "My title".into(),
                },
            ),
            "deleted" => state.dispatch_command(
                cx,
                3,
                Command::DeleteSession {
                    session_id: id.clone(),
                    remove_worktree: false,
                },
            ),
            _ => {}
        }
        if outcome == "failed" {
            scripted
                .events
                .try_send(AgentEvent::Error {
                    message: "unavailable".into(),
                    fatal: true,
                })
                .unwrap();
        } else {
            scripted
                .events
                .try_send(AgentEvent::ItemCompleted(ThreadItem {
                    id: "title".into(),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: raw_title.into(),
                        summarized: false,
                    },
                }))
                .unwrap();
            scripted
                .events
                .try_send(AgentEvent::TurnCompleted {
                    turn_id: "title-turn".into(),
                    status,
                    usage: output_tokens.map(|output_tokens| agent::TokenUsage {
                        output_tokens: Some(output_tokens),
                        ..Default::default()
                    }),
                })
                .unwrap();
        }
        scripted
            .events
            .try_send(AgentEvent::SessionClosed { reason: None })
            .unwrap();
        // Completion arrives after the scratch-cwd cleanup on the thread pool;
        // wait on the pending flag rather than a fixed idle window.
        cx.run_until_seen(|view| !view.title_generating(&id));
        let shown = cx.seen().session(&id);
        assert_eq!(shown.map(|meta| meta.title.as_str()), expected, "{outcome}");
        let persisted = store
            .load_index()
            .unwrap()
            .into_iter()
            .find(|meta| meta.id == id);
        assert_eq!(persisted.as_ref().map(|meta| meta.title.as_str()), expected);
        if !matches!(outcome, "renamed" | "deleted") {
            assert_eq!(shown.unwrap().updated_at, updated_at, "{outcome}");
            assert_eq!(persisted.unwrap().updated_at, updated_at, "{outcome}");
        }
        let sent = cx.drain_outgoing();
        if outcome == "failed" {
            assert!(sent.iter().any(|message| matches!(
                message,
                HostMessage::Event(EventEnvelope {
                    event: ServerEvent::Runtime(RuntimeEvent::Error(
                        RuntimeError::TitleGenerationFailed
                    )),
                    ..
                })
            )));
        }
        // The scratch session the title ran in is stopped once it answered.
        assert!(matches!(
            scripted.commands.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(scripted.commands.try_recv().is_err(), "{outcome}");
    }
    for deleted in [false, true] {
        providers.hold();
        let regenerate = || Command::RegenerateSessionTitle {
            session_id: account_thread.clone(),
        };
        state.command(cx, regenerate()).unwrap();
        let starting = providers.started(cx, launches);
        launches += 1;
        state
            .command(
                cx,
                if deleted {
                    Command::DeleteProfile {
                        profile_id: "titles".into(),
                    }
                } else {
                    Command::UpdateProfileSettings {
                        profile_id: "titles".into(),
                        patch: ProfileSettingsPatch::SetEnabled { enabled: false },
                    }
                },
            )
            .unwrap();
        assert_eq!(
            state.command(cx, regenerate()).unwrap_err().code,
            if deleted {
                "unknown_profile"
            } else {
                "provider_disabled"
            }
        );
        providers.let_go();
        assert!(matches!(
            starting.next_command(cx),
            SessionCommand::Shutdown
        ));
        cx.run_until_seen(|view| !view.title_generating(&account_thread));
        assert!(starting.commands.try_recv().is_err());
        assert_eq!(
            cx.seen().session(&account_thread).unwrap().title,
            "Improve QR Sharing"
        );
        assert!(cx.drain_outgoing().iter().any(|message| matches!(
            message,
            HostMessage::Event(EventEnvelope {
                event: ServerEvent::Runtime(RuntimeEvent::Error(RuntimeError::External(_))),
                ..
            })
        )));
        if !deleted {
            state
                .command(
                    cx,
                    Command::UpdateProfileSettings {
                        profile_id: "titles".into(),
                        patch: ProfileSettingsPatch::SetEnabled { enabled: true },
                    },
                )
                .unwrap();
        }
    }
}

#[test]
fn title_regeneration_rejects_empty_history_without_calling_the_provider() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-empty-title");
    let meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
    let id = meta.id.clone();
    store.upsert_meta(&meta).unwrap();
    let scripted = scripted_provider(ProviderKind::Codex);
    let state = cx.new_entity({
        let mut state = TestClientState::new((*store).clone());
        state.ai_title_generation_enabled = true;
        state.set_provider_launcher(scripted.launcher);
        state
    });
    state.dispatch_command(
        cx,
        1,
        Command::RegenerateSessionTitle {
            session_id: id.clone(),
        },
    );
    cx.run_until(|state| {
        !state
            .index_snapshot()
            .summary
            .title_generating
            .contains(&id)
    });
    assert!(scripted.commands.try_recv().is_err());
    state.read(|state| {
        assert!(
            !state
                .index_snapshot()
                .summary
                .title_generating
                .contains(&id)
        );
        assert_eq!(state.sessions[0].title, meta.title);
    });
    assert!(cx.drain_outgoing().iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            event: ServerEvent::Runtime(RuntimeEvent::Error(RuntimeError::TitleGenerationEmpty)),
            ..
        })
    )));
}

#[test]
fn title_regeneration_context_keeps_the_original_goal_and_recent_messages() {
    let item = |id: &str, content| {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        })
    };
    let first = format!("Original QR sharing goal {}", "界".repeat(3_000));
    let timeline = Timeline::fold_events([
        item(
            "first",
            ItemContent::UserMessage {
                text: first,
                attachments: vec!["/images/first.png".into()],
            },
        ),
        item(
            "reasoning",
            ItemContent::Reasoning {
                text: "private reasoning".into(),
            },
        ),
        item(
            "command",
            ItemContent::CommandExecution {
                command: "cargo test".into(),
                input: Vec::new(),
                output: "tool output".into(),
                exit_code: Some(0),
                status: ItemStatus::Completed,
            },
        ),
        item(
            "middle",
            ItemContent::AssistantMessage {
                text: "intermediate details ".repeat(1_000),
                summarized: false,
            },
        ),
        item(
            "latest",
            ItemContent::UserMessage {
                text: "Focus on QR expiration".into(),
                attachments: (1..=4).map(|i| format!("/images/recent-{i}.png")).collect(),
            },
        ),
        item(
            "answer",
            ItemContent::AssistantMessage {
                text: "Make shared QR links last longer".into(),
                summarized: false,
            },
        ),
    ]);
    let (context, images) = title_regeneration_context(&timeline);
    assert!(context.starts_with("USER:\nOriginal QR sharing goal"));
    assert!(context.contains("USER:\nFocus on QR expiration"));
    assert!(context.ends_with("ASSISTANT:\nMake shared QR links last longer"));
    assert!(!context.contains("private reasoning"));
    assert!(!context.contains("tool output"));
    assert_eq!(
        images,
        [
            "/images/first.png",
            "/images/recent-4.png",
            "/images/recent-3.png",
            "/images/recent-2.png"
        ]
        .map(PathBuf::from)
    );
}

/// A thread forks only while no turn of it runs, background work or not. The
/// fork takes the source's log with its restore points ended, the provider's
/// session to fork from at the next send, and none of the source's pull
/// requests or its worktree, which it shares; a turn a stopped host left open
/// in the source ends in the fork, since no process of the fork can finish
/// it. A source with nothing logged forks from its provider's session alone.
#[test]
fn fork_thread_clones_timeline_and_provider_cursor() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-fork-test");
    let root = test_store.root().clone();
    let source = stored_thread(
        &test_store,
        "source",
        ProviderKind::Codex,
        &root.join("source-worktree"),
        |source| {
            source.model = Some("gpt-5.4".into());
            source.title = "Investigate parser".into();
            source.resume_cursor = Some(agent::ResumeCursor(
                serde_json::json!({"thread_id": "native-source"}),
            ));
            source.worktree = Some(WorktreeInfo {
                root_project_path: root.join("project"),
                base: "main".into(),
                branch: "tcode/source".into(),
            });
            tcode_core::pull_request::link_pull_request(
                &mut source.pull_requests,
                tcode_core::pull_request::PullRequestKey::new("github.com", "sample/project", 1),
                "https://github.com/sample/project/pull/1".into(),
                tcode_core::pull_request::PullRequestSource::Manual,
                1,
                true,
            );
        },
    );
    test_store
        .append_event(
            "source",
            1,
            &AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
        )
        .unwrap();
    let empty_source = stored_thread(
        &test_store,
        "empty-source",
        ProviderKind::Codex,
        &root.join("empty-source"),
        |meta| {
            meta.model = Some("gpt-5.4".into());
            meta.title = "Native conversation without local events".into();
            meta.resume_cursor = Some(agent::ResumeCursor(
                serde_json::json!({"thread_id": "native-empty-source"}),
            ));
        },
    );
    stored_thread(
        &test_store,
        "busy",
        ProviderKind::Codex,
        &root.join("busy"),
        |meta| {
            meta.resume_cursor = Some(agent::ResumeCursor(
                serde_json::json!({"thread_id": "native-busy"}),
            ));
        },
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let fork_of = |state: &TestEntity, cx: &mut TestAppContext, id: &str| {
        state.command(
            cx,
            Command::ForkThread {
                id: id.into(),
                at: None,
            },
        )
    };
    let availability = |cx: &TestAppContext, id: &str| cx.seen().activity(id).unwrap().fork;
    assert_eq!(availability(cx, "source"), ForkAvailability::Available);

    let busy = first_turn(&state, cx, &providers, "busy", 0);
    cx.run_until_seen(|view| view.activity("busy").unwrap().fork == ForkAvailability::Running);
    assert!(fork_of(&state, cx, "busy").is_err());
    busy.play([
        AgentEvent::BackgroundTasksChanged { count: 1 },
        completed_turn("turn"),
    ]);
    cx.run_until_seen(|view| view.activity("busy").unwrap().fork == ForkAvailability::Available);

    let Ok(CommandResponse::SessionId(Some(fork_id))) = fork_of(&state, cx, "source") else {
        panic!("the source forks")
    };
    state.open(cx, &fork_id);
    cx.run_until_seen(|view| view.records(&fork_id).len() == 3);
    let fork = cx.seen().session(&fork_id).unwrap().clone();
    assert_eq!(fork.pending_fork, Some(agent::ForkPoint::Tail));
    assert!(fork.pull_requests.is_empty());
    assert_eq!(
        fork.resume_cursor.as_ref().unwrap().0["thread_id"],
        "native-source"
    );
    assert_eq!(fork.cwd, source.cwd);
    assert_eq!(fork.worktree, None);
    assert!(cx.seen().worktree_shared("source"));
    assert!(!cx.seen().timeline(&fork_id).turn_running);
    // The source's log, then the restore points' end in the new session,
    // and the end of the turn the source left open.
    let records = cx.seen().records(&fork_id).to_vec();
    assert!(matches!(records[0].event, AgentEvent::TurnStarted { .. }));
    assert!(matches!(records[1].event, AgentEvent::CheckpointsCleared));
    assert!(matches!(
        records[2].event,
        AgentEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            ..
        }
    ));

    let Ok(CommandResponse::SessionId(Some(fork_id))) = fork_of(&state, cx, "empty-source") else {
        panic!("a source with nothing logged forks")
    };
    state.open(cx, &fork_id);
    let fork = cx.seen().session(&fork_id).unwrap().clone();
    assert_eq!(fork.pending_fork, Some(agent::ForkPoint::Tail));
    assert_eq!(
        fork.title,
        "Native conversation without local events (fork)"
    );
    assert_eq!(fork.cwd, empty_source.cwd);
    assert_eq!(fork.model, empty_source.model);
    assert_eq!(fork.resume_cursor, empty_source.resume_cursor);
    assert!(cx.seen().timeline(&fork_id).turns.is_empty());
    // Nothing local was copied: only the end of the parent's restore points.
    assert!(matches!(
        cx.seen().records(&fork_id),
        [record] if matches!(record.event, AgentEvent::CheckpointsCleared)
    ));
    assert!(
        test_store
            .read_event_log("empty-source")
            .unwrap()
            .is_empty()
    );
}

/// What a host was told to keep is on disk, in order, once it stops: a
/// thread's metadata and records, and a profile's secret.
#[test]
fn store_writer_flush_persists_ordered_events_metadata_and_secrets_for_reopening() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-writer-reopen");
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    state
        .command(
            cx,
            Command::RenameSession {
                session_id: "thread".into(),
                title: "persisted by writer".into(),
            },
        )
        .unwrap();
    provider.play([persisted_assistant_event("first")]);
    provider.fence(cx, "thread");
    state
        .command(
            cx,
            Command::SetProfileSecret {
                profile_id: Settings::builtin_profile_id(ProviderKind::ClaudeCode).into(),
                name: "ANTHROPIC_API_KEY".into(),
                value: Some("writer-secret".into()),
            },
        )
        .unwrap();
    // More than one batch's worth, so the flush waits for several commits.
    provider.play(
        std::iter::once(persisted_assistant_event("second"))
            .chain((0..600).map(|index| persisted_assistant_event(&format!("bulk {index}")))),
    );
    provider.fence(cx, "thread");
    state.command(cx, Command::ShutdownAllAndFlush).unwrap();

    let fresh = SessionStore::open_at(test_store.root().clone()).unwrap();
    assert_eq!(
        fresh
            .load_index()
            .unwrap()
            .iter()
            .find(|meta| meta.id == "thread")
            .unwrap()
            .title,
        "persisted by writer"
    );
    let texts: Vec<_> = fresh
        .read_events("thread")
        .unwrap()
        .into_iter()
        .filter_map(|stored| match stored.event {
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text, .. },
                ..
            }) => Some(text),
            _ => None,
        })
        .collect();
    let bulk: Vec<_> = (0..600).map(|index| format!("bulk {index}")).collect();
    assert_eq!(texts[..2], ["first", "second"]);
    assert_eq!(texts[2..], bulk);
    assert_eq!(
        SettingsStore::new(test_store.root().clone())
            .profile_secrets(Settings::builtin_profile_id(ProviderKind::ClaudeCode))
            .unwrap()
            .get("ANTHROPIC_API_KEY")
            .map(String::as_str),
        Some("writer-secret")
    );
}
