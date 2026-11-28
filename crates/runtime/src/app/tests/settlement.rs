use super::*;

#[test]
fn cold_lifecycle_migrates_legacy_settlement_and_ages_from_events_after_rename() {
    use crate::pipe::{HostServices, spawn_host};
    let store = TestStore::new("cold-settlement");
    // The threads' activity was ten days before the host starts.
    let clock = test_clock();
    let stamp = clock.host_clock().unix_millis();
    clock.advance(Duration::from_secs(10 * 86_400));
    let services = || HostServices {
        clock: clock.host_clock(),
        ..HostServices::default()
    };
    for id in ["inherit", "never"] {
        let mut project = Project::from_root(store.root().join(id));
        project.id = id.into();
        store.upsert_project(&project).unwrap();
    }
    for (id, settled) in [
        ("legacy", Some(7)),
        ("aged", None),
        ("untouched", None),
        ("never", None),
    ] {
        let meta: SessionMeta = serde_json::from_value(serde_json::json!({
            "id": id, "title": id, "provider": "codex", "cwd": store.root(),
            "created_at": stamp / 1000, "updated_at": stamp / 1000, "settled_at": settled,
            "project_id": if id == "never" { "never" } else { "inherit" }
        }))
        .unwrap();
        store.upsert_meta(&meta).unwrap();
    }
    for id in ["aged", "never"] {
        store
            .append_event(
                id,
                stamp,
                &AgentEvent::ItemCompleted(ThreadItem {
                    id: "old-human".into(),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: "An old request".into(),
                        attachments: vec![],
                    },
                }),
            )
            .unwrap();
    }
    // Literal persisted settings distinguish an omitted project value from
    // an explicit null.
    std::fs::write(
        store.root().join("settings.json"),
        r#"{
        "auto_settle_after_days":null,
        "project_settlement_overrides":{"inherit":{},"never":{"auto_settle_after_days":null}}
    }"#,
    )
    .unwrap();
    let host = spawn_host((*store).clone(), services()).unwrap();
    let link = host.link();
    let events = crate::pipe::p4b_tests::observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    })
    .unwrap();
    link.command_blocking(Command::RenameSession {
        session_id: "aged".into(),
        title: "Recently renamed".into(),
    })
    .unwrap();
    for patch in [
        SettingsPatch::AutoSettleAfterDays(Some(0.5)),
        SettingsPatch::ProjectSettlement {
            project_id: "never".into(),
            value: Some(tcode_core::settings::ProjectSettlementSettings {
                auto_settle_after_days: Some(Some(91.0)),
                ..Default::default()
            }),
        },
    ] {
        assert_eq!(
            link.command_blocking(Command::PatchSettings { patch })
                .unwrap_err()
                .code,
            "invalid_settings"
        );
    }
    link.command_blocking(Command::PatchSettings {
        patch: SettingsPatch::AutoSettleAfterDays(Some(3.0)),
    })
    .unwrap();
    loop {
        if let ServerEvent::IndexUpsertSession(meta) = next_host_event(&events).event
            && meta.id == "aged"
            && meta.is_settled()
        {
            assert_eq!(meta.settled_at, Some(stamp / 1000));
            break;
        }
    }
    link.shutdown_blocking().unwrap();
    host.to_host.close();
    host.stopped.recv_blocking().unwrap();
    let restarted = spawn_host(
        SessionStore::open_at(store.root().clone()).unwrap(),
        services(),
    )
    .unwrap();
    let link = restarted.link();
    let events = crate::pipe::p4b_tests::observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    })
    .unwrap();
    loop {
        if let ServerEvent::IndexSnapshot(snapshot) = next_host_event(&events).event {
            let settled = settled_threads(&link);
            let find = |id: &str| {
                snapshot
                    .sessions
                    .iter()
                    .chain(&settled.sessions)
                    .find(|meta| meta.id == id)
                    .unwrap()
            };
            assert_eq!(
                find("legacy").settled_override,
                Some(tcode_core::project::SettledOverride::Settled)
            );
            assert_eq!(find("legacy").settled_at, Some(7));
            assert!(find("aged").is_settled());
            assert!(
                !find("never").is_settled(),
                "explicit project null survives settings save and restart"
            );
            assert!(
                !find("untouched").is_settled(),
                "no event activity never ages"
            );
            break;
        }
    }
    let inherited: Command = serde_json::from_str(r#"{"type":"patch_settings","content":{"patch":{"type":"project_settlement","content":{"project_id":"never","value":{}}}}}"#).unwrap();
    link.command_blocking(inherited).unwrap();
    loop {
        if let ServerEvent::IndexUpsertSession(meta) = next_host_event(&events).event
            && meta.id == "never"
            && meta.is_settled()
        {
            assert_eq!(meta.settled_at, Some(stamp / 1000));
            break;
        }
    }
    link.shutdown_blocking().unwrap();
    restarted.to_host.close();
    restarted.stopped.recv_blocking().unwrap();
}

/// The settled threads a client reads beside the index, all of them.
fn settled_threads(link: &tcode_client::HostLink) -> tcode_protocol::SessionPage {
    let settled = smol::block_on(link.query(tcode_protocol::Query::SettledSessions {
        scope: Default::default(),
        before: None,
        limit: u32::MAX,
    }));
    match settled {
        Ok(tcode_protocol::QueryResponse::SettledSessions(page)) => page,
        other => panic!("the settled threads could not be read: {other:?}"),
    }
}

/// A subscriber's index carries the threads that are not settled, with
/// what they started, and counts the settled ones. Those are read a page at
/// a time, the latest settled first, and a thread outside the index with
/// the thread it descends from and everything that one started.
#[test]
fn the_index_leaves_settled_threads_to_pages_and_families() {
    use tcode_protocol::{Query, QueryResponse, SettledCursor, SettledScope};
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("index-settled-pages");
    let thread = |id: &str, settled: Option<u64>, parent: Option<&str>| {
        stored_thread(&store, id, ProviderKind::Codex, store.root(), |meta| {
            meta.project_id = Some("project".into());
            meta.settled_at = settled;
            meta.settled_override = settled.map(|_| tcode_core::project::SettledOverride::Settled);
            meta.parent_session_id = parent.map(str::to_owned);
        });
    };
    thread("active", None, None);
    thread("active-child", Some(5), Some("active"));
    thread("old", Some(10), None);
    thread("old-child", None, Some("old"));
    thread("older", Some(5), None);
    thread("newest", Some(20), None);
    let (state, _providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);

    let ids = |sessions: &[SessionMeta]| -> Vec<String> {
        sessions.iter().map(|meta| meta.id.clone()).collect()
    };
    let snapshot = cx
        .drain_outgoing()
        .into_iter()
        .find_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                event: ServerEvent::IndexSnapshot(snapshot),
                ..
            }) => Some(snapshot),
            _ => None,
        })
        .expect("the subscriber's index");
    let mut listed = ids(&snapshot.sessions);
    listed.sort();
    assert_eq!(listed, ["active", "active-child"]);
    let mut active: Vec<_> = snapshot.summary.activity.keys().cloned().collect();
    active.sort();
    assert_eq!(active, listed);
    assert_eq!(snapshot.summary.settled_counts["project"], 3);

    let page = |cx: &mut TestAppContext, before: Option<SettledCursor>| {
        let query = Query::SettledSessions {
            scope: SettledScope::default(),
            before,
            limit: 2,
        };
        match state.query(cx, query).unwrap() {
            QueryResponse::SettledSessions(page) => page,
            other => panic!("unexpected response: {other:?}"),
        }
    };
    let first = page(cx, None);
    assert_eq!(ids(&first.sessions), ["newest", "old"]);
    assert!(first.activity.contains_key("old"));
    let rest = page(
        cx,
        Some(SettledCursor {
            at: 10,
            id: "old".into(),
        }),
    );
    assert_eq!(ids(&rest.sessions), ["older"]);

    let family = state.query(
        cx,
        Query::ThreadFamily {
            session_id: "old-child".into(),
        },
    );
    match family.unwrap() {
        QueryResponse::ThreadFamily(family) => {
            assert_eq!(ids(&family.sessions), ["old", "old-child"])
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn first_index_snapshot_includes_persisted_child_runs_and_human_resume_status() {
    use tcode_core::pull_request::{
        Mergeability, PullRequestKey, PullRequestSnapshot, PullRequestSource, PullRequestState,
        link_pull_request,
    };
    use tcode_services::store::Mutation;

    let store = TestStore::new("startup-human-clock");
    let hour = 3_600_000;
    // The thread was created ten hours before the host starts.
    let clock = test_clock();
    let created = clock.host_clock().unix_millis();
    clock.advance(Duration::from_secs(10 * 3_600));
    let merged = created + 2 * hour;
    let resumed = created + 4 * hour;
    let completed = created + 5 * hour;
    let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
    meta.id = "resumed".into();
    meta.created_at = created / 1000;
    link_pull_request(
        &mut meta.pull_requests,
        PullRequestKey::new("github.com", "sample/project", 1),
        "https://github.com/sample/project/pull/1".into(),
        PullRequestSource::Agent,
        created / 1000,
        true,
    );
    let merged_at = chrono::DateTime::from_timestamp_millis(merged as i64)
        .unwrap()
        .to_rfc3339();
    meta.pull_requests[0].snapshot = Some(PullRequestSnapshot {
        state: PullRequestState::Merged,
        title: "Sample change".into(),
        head_branch: "sample-change".into(),
        base_branch: "main".into(),
        is_draft: false,
        updated_at: Some(merged_at.clone()),
        synced_at: merged / 1000,
        closed_at: Some(merged_at.clone()),
        merged_at: Some(merged_at),
        author: None,
        additions: Some(1),
        deletions: Some(1),
        changed_files: Some(1),
        review_decision: None,
        checks_state: None,
        mergeability: Mergeability::Unknown,
    });
    store.upsert_meta(&meta).unwrap();
    for (id, parent) in [("lead", None), ("worker", Some("lead"))] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_owned);
        meta.created_at = created / 1000;
        store.upsert_meta(&meta).unwrap();
    }
    let mut log = vec![
        (
            resumed,
            AgentEvent::ItemCompleted(ThreadItem {
                id: "resume".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "One more change".into(),
                    attachments: vec![],
                },
            }),
        ),
        (
            resumed + 1,
            AgentEvent::TurnStarted {
                turn_id: "resumed-run".into(),
            },
        ),
    ];
    log.extend((0..300).map(|step| {
        (
            resumed + 2 + step,
            persisted_assistant_event(&format!("step {step}")),
        )
    }));
    log.push((
        completed,
        AgentEvent::TurnCompleted {
            turn_id: "resumed-run".into(),
            status: TurnStatus::Completed,
            usage: None,
        },
    ));
    for (id, sender) in [
        ("resumed", MessageOrigin::Human),
        ("worker", MessageOrigin::Agent),
    ] {
        let mutations: Vec<_> = log
            .iter()
            .map(|(ts, event)| {
                let origin = matches!(
                    event,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::UserMessage { .. },
                        ..
                    })
                )
                .then_some(sender);
                Mutation::append_authored_event(id, *ts, event, None, origin, |_| None).unwrap()
            })
            .collect();
        store.apply(&mutations).unwrap();
    }

    let host = crate::pipe::spawn_host(
        (*store).clone(),
        crate::pipe::HostServices {
            clock: clock.host_clock(),
            ..crate::pipe::HostServices::default()
        },
    )
    .unwrap();
    let link = host.link();
    let events = crate::pipe::p4b_tests::observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    })
    .unwrap();
    loop {
        if let ServerEvent::IndexSnapshot(snapshot) = next_host_event(&events).event {
            assert_eq!(snapshot.summary.activity.len(), 3);
            assert!(
                !snapshot
                    .sessions
                    .iter()
                    .find(|meta| meta.id == "resumed")
                    .unwrap()
                    .is_settled()
            );
            assert!(children_hold(&snapshot.summary.activity["lead"]));
            let worker = snapshot.summary.activity["worker"].agent.unwrap();
            assert_eq!(
                (worker.run_started_at, worker.run_completed_at),
                (Some(resumed + 1), Some(completed))
            );
            assert_eq!(
                worker.execution,
                tcode_core::settlement::AgentExecution::Finished
            );
            break;
        }
    }
    link.shutdown_blocking().unwrap();
    host.to_host.close();
    host.stopped.recv_blocking().unwrap();
}

/// A thread whose harness still runs background work says the host will not
/// settle it, and settling it is refused.
#[test]
fn a_thread_the_host_will_not_settle_says_so_in_its_activity() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("settle-blocked");
    stored_thread(
        &store,
        "background",
        ProviderKind::Codex,
        &store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    let provider = first_turn(&state, cx, &providers, "background", 0);
    provider.play([
        AgentEvent::BackgroundTasksChanged { count: 1 },
        completed_turn("turn"),
    ]);
    provider.fence(cx, "background");
    let activity = cx.seen().activity("background").unwrap();
    assert!(!activity.working);
    assert!(activity.settle_blocked);
    let refused = state
        .command(
            cx,
            Command::SettleSession {
                session_id: "background".into(),
            },
        )
        .unwrap_err();
    assert_eq!(refused.code, "thread_busy");
}

/// A thread whose turn runs cannot be archived.
#[test]
fn archiving_a_thread_with_a_turn_in_flight_is_refused() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("archive-busy");
    stored_thread(
        &store,
        "busy",
        ProviderKind::Codex,
        &store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    first_turn(&state, cx, &providers, "busy", 0);
    let refused = state
        .command(
            cx,
            Command::ArchiveSession {
                session_id: "busy".into(),
            },
        )
        .unwrap_err();
    assert_eq!(refused.code, "thread_busy");
    assert_eq!(cx.seen().session("busy").unwrap().archived_at, None);
}

#[test]
fn forking_a_thread_that_cannot_be_forked_is_refused_with_the_reason() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("fork-refused");
    let mut empty = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
    empty.id = "empty".into();
    store.upsert_meta(&empty).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));

    let answered = through_pipe(
        &state,
        cx,
        1,
        tcode_protocol::ClientPayload::Command(Command::ForkThread {
            id: "empty".into(),
            at: None,
        }),
    );
    assert!(answered.iter().any(|message| matches!(
        message,
        HostMessage::Ack { id: 1, result: Err(error) }
            if error.code == "fork_unavailable"
    )));
}

#[test]
fn a_relaunch_marker_the_host_cannot_write_is_refused_with_the_reason() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("relaunch-unwritable");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    // A directory where the marker goes: the write fails as a full or
    // read-only disk would.
    std::fs::create_dir_all(store.root().join("relaunch.json")).unwrap();
    let answered = through_pipe(
        &state,
        cx,
        1,
        tcode_protocol::ClientPayload::Command(Command::WriteRelaunchMarker {
            session_id: None,
            reopen_settings: "computer_use".into(),
        }),
    );
    assert!(answered.iter().any(|message| matches!(
        message,
        HostMessage::Ack { id: 1, result: Err(error) } if error.code == "relaunch_marker"
    )));
}

/// A thread settles only by itself and only when nothing of it waits on the
/// user: a message the user queued, an approval or a blocking question keeps
/// it out, a question its harness does not wait on does not. Settling stops
/// its harness and sends nothing of what was queued for it automatically, a
/// lead's waking report included, and leaves its children running. Reading
/// it, archiving and unarchiving it and reopening it keep it settled;
/// un-settling it holds it active until a message it accepts makes it
/// ordinarily unsettled again, which a host started again reads.
#[test]
fn manual_settlement_is_thread_local_and_waits_for_the_queue_and_the_user() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("manual-settlement");
    stored_thread(
        &store,
        "parent",
        ProviderKind::Codex,
        &store.root().join("parent"),
        |meta| {
            meta.unsettled_at = Some(1);
        },
    );
    stored_thread(
        &store,
        "child",
        ProviderKind::Codex,
        &store.root().join("child"),
        |meta| {
            meta.parent_session_id = Some("parent".into());
            meta.expectation = Some(ChildExpectation::opened(1));
        },
    );
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    let settle = |cx: &mut TestAppContext| {
        state.command(
            cx,
            Command::SettleSession {
                session_id: "parent".into(),
            },
        )
    };
    let busy = |result: Result<CommandResponse, ProtocolError>| matches!(result, Err(error) if error.code == "thread_busy");
    // The child's turn runs on.
    first_turn(&state, cx, &providers, "child", 0);
    state.leave(cx, "child");
    let parent = first_turn(&state, cx, &providers, "parent", 1);
    parent.play([completed_turn("turn")]);
    parent.fence(cx, "parent");

    // A message queued for later keeps the thread out, the user's or the host's.
    state
        .command(
            cx,
            Command::ScheduleTurn {
                session_id: "parent".into(),
                text: "human later".into(),
                attachment_paths: vec![],
                due: tcode_protocol::TurnDue::AfterSecs(3600),
            },
        )
        .unwrap();
    assert!(busy(settle(cx)));
    let human = cx.seen().status("parent").queued_messages[0].id;
    state
        .command(
            cx,
            Command::DropQueued {
                session_id: "parent".into(),
                id: human,
            },
        )
        .unwrap();
    // So does what the harness waits on the user for.
    for (request, resolved) in [
        (
            AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval".into(),
                parent_item_id: Some("toolu_agent".into()),
                turn_id: None,
                kind: agent::ApprovalKind::ToolUse {
                    name: "shell".into(),
                    input: serde_json::json!({}),
                    detail: "Needs consent".into(),
                },
                reason: None,
                blocked_path: None,
                options: vec![],
            }),
            AgentEvent::ApprovalResolved {
                request_id: "approval".into(),
                decision: agent::ApprovalDecision::Option("reject".into()),
            },
        ),
        (
            AgentEvent::UserInputRequested {
                request_id: "blocking".into(),
                parent_item_id: None,
                questions: vec![],
                delivery: agent::UserInputDelivery::Blocking,
            },
            AgentEvent::UserInputResolved {
                request_id: "blocking".into(),
                answers: Default::default(),
            },
        ),
    ] {
        parent.play([request]);
        parent.fence(cx, "parent");
        assert!(busy(settle(cx)));
        parent.play([resolved]);
        parent.fence(cx, "parent");
    }
    // The harness hit its usage limit: the host queues the resume for when
    // it resets, on its own.
    parent.play([AgentEvent::UsageLimitReached {
        resets_at: cx.now_secs() + 3_600,
    }]);
    parent.fence(cx, "parent");
    let resume = cx.seen().status("parent").queued_messages[0].clone();
    assert_eq!(resume.text, tcode_core::session::RESUME_PROMPT);
    let activity = cx.seen().activity("parent").unwrap();
    assert!(activity.settle_blocked && activity.wakes.scheduled_at.is_some());
    assert!(busy(settle(cx)));
    state
        .command(
            cx,
            Command::DropQueued {
                session_id: "parent".into(),
                id: resume.id,
            },
        )
        .unwrap();
    parent.play([AgentEvent::UserInputRequested {
        request_id: "asynchronous".into(),
        parent_item_id: None,
        questions: vec![],
        delivery: agent::UserInputDelivery::Async,
    }]);
    parent.fence(cx, "parent");
    settle(cx).unwrap();
    assert!(matches!(parent.next_command(cx), SessionCommand::Shutdown));
    let settled = |cx: &TestAppContext, id: &str| cx.seen().session(id).unwrap().is_settled();
    assert!(settled(cx, "parent"));
    assert!(cx.seen().session("parent").unwrap().unsettled_at.is_none());
    assert!(cx.seen().activity("child").unwrap().turn_running);
    assert!(!settled(cx, "child"));

    // Reading, archiving and coming back keep it settled.
    state.leave(cx, "parent");
    state.subscribe(
        cx,
        Topic::SessionEvents {
            session_id: "parent".into(),
        },
    );
    assert!(settled(cx, "parent"), "reading leaves settlement intact");
    for command in [
        Command::ArchiveSession {
            session_id: "parent".into(),
        },
        Command::UnarchiveSession {
            session_id: "parent".into(),
        },
    ] {
        state.command(cx, command).unwrap();
    }
    cx.run_until_seen(|view| view.session("parent").is_some());
    assert!(settled(cx, "parent"));
    state.open(cx, "parent");
    assert!(
        settled(cx, "parent"),
        "reopening status preserves settlement"
    );

    state
        .command(
            cx,
            Command::UnsettleSession {
                session_id: "parent".into(),
            },
        )
        .unwrap();
    assert_eq!(
        cx.seen().session("parent").unwrap().settled_override,
        Some(tcode_core::project::SettledOverride::Active)
    );
    state.send(cx, "parent", "new work");
    let next = providers.started(cx, 2);
    let (delivery_id, text) = next.next_turn(cx);
    assert_eq!(text, "new work");
    next.play([AgentEvent::TurnAccepted { delivery_id }]);
    cx.run_until_seen(|view| view.session("parent").unwrap().settled_override.is_none());
    let meta = cx.seen().session("parent").unwrap();
    assert!(!meta.is_settled());
    assert!(
        meta.unsettled_at.is_some(),
        "an accepted message clears explicit Active"
    );
    state.command(cx, Command::ShutdownAllAndFlush).unwrap();
    let restarted = AppState::new((*store).clone()).unwrap();
    assert!(!restarted.find_meta("parent").unwrap().is_settled());
}

/// A report wakes its settled lead, renamed or not, and no thread above it;
/// a lead archived or deleted before the report, or a child archived before
/// it ran, is woken by nothing.
#[test]
fn a_report_reopens_only_its_lead_and_never_reaches_an_archived_or_deleted_one() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("completion-admission");
    let cases = [
        "valid",
        "renamed",
        "archived_child",
        "archived_parent",
        "deleted_parent",
    ];
    for case in cases {
        let ancestor = format!("{case}-ancestor");
        let parent = format!("{case}-parent");
        let child = format!("{case}-child");
        stored_thread(
            &store,
            &ancestor,
            ProviderKind::Codex,
            &store.root().join(&ancestor),
            |meta| {
                meta.settled_at = Some(1);
            },
        );
        stored_thread(
            &store,
            &parent,
            ProviderKind::Codex,
            &store.root().join(&parent),
            |meta| {
                meta.parent_session_id = Some(ancestor.clone());
                meta.settled_at = Some(1);
            },
        );
        stored_thread(
            &store,
            &child,
            ProviderKind::Codex,
            &store.root().join(&child),
            |meta| {
                meta.parent_session_id = Some(parent.clone());
                meta.expectation = Some(ChildExpectation::opened(1));
            },
        );
    }
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    let mut launches = 0;
    for case in cases {
        let (ancestor, parent, child) = (
            format!("{case}-ancestor"),
            format!("{case}-parent"),
            format!("{case}-child"),
        );
        // Each command lands before the child runs and reports.
        let command = match case {
            "renamed" => Some(Command::RenameSession {
                session_id: child.clone(),
                title: "Renamed".into(),
            }),
            "archived_child" => Some(Command::ArchiveSession {
                session_id: child.clone(),
            }),
            "archived_parent" => Some(Command::ArchiveSession {
                session_id: parent.clone(),
            }),
            "deleted_parent" => Some(Command::DeleteSession {
                session_id: parent.clone(),
                remove_worktree: false,
            }),
            _ => None,
        };
        if let Some(command) = command {
            state.command(cx, command).unwrap();
        }
        if cx.seen().session(&child).is_some() {
            let played = first_turn(&state, cx, &providers, &child, launches);
            launches += 1;
            played.play([
                report_result_event("rep", "completion evidence"),
                completed_turn("turn"),
            ]);
            played.fence(cx, &child);
        }
        if case == "valid" || case == "renamed" {
            let woken = providers.started(cx, launches);
            launches += 1;
            assert_eq!(woken.options.cwd, store.root().join(&parent));
            assert!(woken.next_turn(cx).1.contains("completion evidence"));
            assert!(!cx.seen().session(&parent).unwrap().is_settled());
        } else {
            cx.run_until_parked();
            if let Some(parent) = cx.seen().session(&parent) {
                assert!(parent.is_settled(), "{case}");
            }
            assert_eq!(providers.count(), launches, "{case}: nothing is woken");
        }
        assert!(cx.seen().session(&ancestor).unwrap().is_settled(), "{case}");
    }
}

/// Settling a thread whose harness runs background work is refused and stops
/// nothing; once the work ends it settles.
#[test]
fn manual_settle_waits_for_the_harness_background_work() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("settle-background-work");
    stored_thread(
        &store,
        "shell",
        ProviderKind::ClaudeCode,
        &store.root().join("shell"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    let provider = first_turn(&state, cx, &providers, "shell", 0);
    provider.play([
        AgentEvent::BackgroundTasksChanged { count: 1 },
        completed_turn("turn"),
    ]);
    provider.fence(cx, "shell");
    let settle = || Command::SettleSession {
        session_id: "shell".into(),
    };
    assert_eq!(state.command(cx, settle()).unwrap_err().code, "thread_busy");
    assert!(!cx.seen().session("shell").unwrap().is_settled());
    assert!(
        next_command(&provider.commands).is_err(),
        "a refused settle leaves the harness and its background work running"
    );

    provider.play([AgentEvent::BackgroundTasksChanged { count: 0 }]);
    provider.fence(cx, "shell");
    state.command(cx, settle()).unwrap();
    cx.run_until_seen(|view| view.session("shell").unwrap().is_settled());
}

#[test]
fn a_finished_child_holds_its_lead_until_settled_and_cancel_never_delivers() {
    use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};
    use tcode_protocol::AgentStatus;

    let cx = &mut TestAppContext::default();
    let store = TestStore::new("agent-delivery");
    let stamp = cx.now_millis();
    cx.clock.advance(Duration::from_secs(10 * 86_400));
    // A removed directory makes the reopened child's provider start fail
    // instead of spawning a real CLI.
    let gone = std::env::temp_dir().join(format!("tcode-removed-{}", uuid::Uuid::new_v4()));
    for (id, parent) in [
        ("lead", None),
        ("worker", Some("lead")),
        ("quitter", Some("lead")),
    ] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, gone.clone(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_owned);
        meta.created_at = stamp / 1000;
        store.upsert_meta(&meta).unwrap();
        for event in [
            AgentEvent::TurnStarted {
                turn_id: "run".into(),
            },
            persisted_assistant_event("report"),
            AgentEvent::TurnCompleted {
                turn_id: "run".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ] {
            store.append_event(id, stamp, &event).unwrap();
        }
    }
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let activity = |state: &TestEntity, id: &str| {
        state.read(|state| state.index_snapshot().summary.activity[id].clone())
    };
    // The lead's tcode_control calls.
    let op = |state: &TestEntity, cx: &mut TestAppContext, op| {
        let (reply, response) = smol::channel::bounded(1);
        state.update(cx, |state, cx| {
            state.handle_control_call("lead".into(), op, None, reply, cx)
        });
        cx.run_until_recv(&response)
    };
    let applied = |answer: Result<serde_json::Value, String>| {
        let answer = answer.unwrap();
        assert_eq!(answer["results"][0]["outcome"], "applied", "{answer}");
    };
    // Every run here started and ended at `stamp`.
    let agent = |execution, delivery| {
        Some(AgentStatus {
            execution,
            delivery,
            report: AgentReport::None,
            run_started_at: Some(stamp),
            run_completed_at: Some(stamp),
        })
    };

    state.update(cx, |state, _| state.settings.auto_settle_after_days = None);
    state.dispatch_command(
        cx,
        1,
        Command::PatchSettings {
            patch: SettingsPatch::AutoSettleAfterDays(Some(3.0)),
        },
    );
    cx.run_until(|state| {
        ["lead", "worker", "quitter"]
            .iter()
            .all(|id| state.thread_activity.contains_key(*id))
    });
    state.read(|state| {
        for id in ["lead", "worker", "quitter"] {
            assert!(!state.find_meta(id).unwrap().is_settled(), "{id}");
        }
    });
    assert!(children_hold(&activity(&state, "lead")));
    assert_eq!(
        activity(&state, "worker").agent,
        agent(AgentExecution::Finished, AgentDelivery::AwaitingSettle)
    );
    assert_eq!(activity(&state, "lead").agent, None);

    applied(op(
        &state,
        cx,
        control_mcp::ControlOp::ControlThread(control_mcp::ControlThreadParams {
            changes: vec![control_mcp::RunChange {
                thread_id: "quitter".into(),
                op: control_mcp::RunOp::Cancel,
            }],
            request_id: None,
        }),
    ));
    state.read(|state| assert!(!state.find_meta("quitter").unwrap().is_settled()));
    assert_eq!(
        activity(&state, "quitter").agent,
        agent(AgentExecution::Cancelled, AgentDelivery::NotDelivered)
    );
    assert!(
        children_hold(&activity(&state, "lead")),
        "the unsettled worker still holds its lead"
    );
    let mut delivery = |id: &str| {
        op(
            &state,
            cx,
            control_mcp::ControlOp::GetThread(control_mcp::GetThreadParams {
                thread_id: id.into(),
            }),
        )
        .unwrap()["activity"]["agent"]["delivery"]
            .clone()
    };
    assert_eq!(delivery("worker"), "awaiting_settle");
    assert_eq!(delivery("quitter"), "not_delivered");

    applied(op(
        &state,
        cx,
        control_mcp::ControlOp::UpdateThreads(control_mcp::UpdateThreadsParams {
            changes: vec![control_mcp::ThreadChange {
                thread_id: "worker".into(),
                op: control_mcp::ThreadOp::Settle,
                expect: None,
            }],
            dry_run: None,
        }),
    ));
    assert_eq!(
        activity(&state, "worker").agent,
        agent(AgentExecution::Finished, AgentDelivery::Settled)
    );
    assert!(!children_hold(&activity(&state, "lead")));
    state.dispatch_command(
        cx,
        2,
        Command::PatchSettings {
            patch: SettingsPatch::AutoSettleAfterDays(Some(2.0)),
        },
    );
    cx.run_until(|state| {
        state
            .find_meta("lead")
            .is_some_and(|meta| meta.settled_at == Some(stamp / 1000))
    });

    applied(op(
        &state,
        cx,
        control_mcp::ControlOp::SendMessage(control_mcp::SendMessageParams {
            thread_ids: vec!["worker".into()],
            text: "One more check".into(),
            mode: control_mcp::SendMode::Queue,
            attachments: None,
            request_id: None,
        }),
    ));
    state.read(|state| assert!(!state.find_meta("worker").unwrap().is_settled()));
    assert_ne!(
        activity(&state, "worker").agent.unwrap().delivery,
        AgentDelivery::Settled,
        "a message reopens the settled child"
    );
}

/// Archiving an agent its lead never settled cancels it from the lead's point
/// of view, so restoring it later (as Open in the Agents view does) shows it
/// not delivered and never holds the lead again.
#[test]
fn an_agent_archived_before_settle_stays_not_delivered_when_restored() {
    use tcode_core::settlement::{AgentDelivery, AgentExecution};

    let cx = &mut TestAppContext::default();
    let store = TestStore::new("agent-archived-before-settle");
    let stamp = cx.now_millis();
    cx.clock.advance(Duration::from_secs(60));
    for (id, parent) in [("lead", None), ("worker", Some("lead"))] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_owned);
        store.upsert_meta(&meta).unwrap();
        for event in [
            AgentEvent::TurnStarted {
                turn_id: "run".into(),
            },
            persisted_assistant_event("report"),
            AgentEvent::TurnCompleted {
                turn_id: "run".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ] {
            store.append_event(id, stamp, &event).unwrap();
        }
    }
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let activity = |state: &TestEntity| state.read(|state| state.index_snapshot().summary.activity);
    assert!(
        children_hold(&activity(&state)["lead"]),
        "the finished worker awaits settle"
    );

    state.dispatch_command(
        cx,
        1,
        Command::ArchiveSession {
            session_id: "worker".into(),
        },
    );
    cx.run_until_parked();
    assert!(!children_hold(&activity(&state)["lead"]));

    state.dispatch_command(
        cx,
        2,
        Command::UnarchiveSession {
            session_id: "worker".into(),
        },
    );
    cx.run_until_parked();
    let activity = activity(&state);
    let worker = activity["worker"].agent.unwrap();
    assert_eq!(
        (worker.execution, worker.delivery),
        (AgentExecution::Cancelled, AgentDelivery::NotDelivered)
    );
    assert!(
        !children_hold(&activity["lead"]),
        "the restored worker does not hold its lead"
    );
}

/// Settling a dispatched child stops its own harness and the subagent that
/// harness ran, leaves the work of its own children running, and lets its
/// lead settle once nothing under the lead is unsettled.
#[test]
fn settling_detaches_only_its_own_provider_and_releases_its_parent() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("settlement-own-provider");
    let stamp = cx.now_millis();
    cx.clock.advance(Duration::from_secs(10 * 86_400));
    stored_thread(
        &store,
        "lead",
        ProviderKind::Codex,
        &store.root().join("lead"),
        |lead| {
            lead.created_at = stamp / 1000;
        },
    );
    store
        .append_event(
            "lead",
            stamp,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "old-request".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "Earlier work".into(),
                    attachments: vec![],
                },
            }),
        )
        .unwrap();
    for (id, parent) in [("worker", "lead"), ("independent", "worker")] {
        stored_thread(
            &store,
            id,
            ProviderKind::Codex,
            &store.root().join(id),
            |meta| {
                meta.parent_session_id = Some(parent.into());
            },
        );
    }
    let settings = Settings {
        auto_settle_after_days: None,
        ..Default::default()
    };
    SettingsStore::new(store.root().clone())
        .save(&settings)
        .unwrap();
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    let spawn = |owner: &str| {
        AgentEvent::ItemStarted(ThreadItem {
            id: format!("spawn-{owner}"),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: "explorer".into(),
                description: "Inspect sample".into(),
                status: ItemStatus::InProgress,
                summary: None,
                model: None,
                effort: None,
            },
        })
    };
    // The independent child's turn runs on; the worker's has ended, its
    // subagent going on in the background.
    let independent = first_turn(&state, cx, &providers, "independent", 0);
    independent.play([spawn("independent")]);
    independent.fence(cx, "independent");
    state.leave(cx, "independent");
    let worker = first_turn(&state, cx, &providers, "worker", 1);
    worker.play([spawn("worker"), completed_turn("turn")]);
    worker.fence(cx, "worker");

    // An unsettled dispatched child holds its lead out of settlement.
    state
        .command(
            cx,
            Command::PatchSettings {
                patch: SettingsPatch::AutoSettleAfterDays(Some(3.0)),
            },
        )
        .unwrap();
    assert!(!cx.seen().session("lead").unwrap().is_settled());
    state
        .command(
            cx,
            Command::SettleSession {
                session_id: "worker".into(),
            },
        )
        .unwrap();
    assert!(matches!(worker.next_command(cx), SessionCommand::Shutdown));
    let mirror = |spawn: &str| cx.seen().mirror(spawn).unwrap().id.clone();
    let (worker_mirror, independent_mirror) = (mirror("spawn-worker"), mirror("spawn-independent"));
    state.open(cx, &worker_mirror);
    assert_eq!(
        cx.seen().timeline(&worker_mirror).last_turn_status,
        Some(TurnStatus::Interrupted)
    );
    assert!(
        cx.seen()
            .activity(&independent_mirror)
            .unwrap()
            .turn_running
    );
    assert!(cx.seen().activity("independent").unwrap().turn_running);
    assert!(next_command(&independent.commands).is_err());

    state
        .command(
            cx,
            Command::PatchSettings {
                patch: SettingsPatch::AutoSettleAfterDays(Some(2.0)),
            },
        )
        .unwrap();
    cx.run_until_seen(|view| view.session("lead").unwrap().settled_at == Some(stamp / 1000));
}

/// A pin promotes a settled thread without reopening it, holds an idle
/// thread out of automatic settlement, and a settle removes the pin and the
/// thread's place in the active order. None of them, nor un-settling or
/// the auto-settle toggle, is activity, so none moves the thread in the
/// project view.
#[test]
fn pinning_promotes_and_exempts_until_a_settle_unpins() {
    use tcode_core::project::SettledOverride;
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("pinned-threads");
    let stamp = cx.now_millis();
    cx.clock.advance(Duration::from_secs(10 * 86_400));
    for (id, settled) in [("parked", true), ("idle", false), ("arranged", false)] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
        meta.id = id.into();
        meta.created_at = stamp / 1000;
        meta.updated_at = stamp / 1000;
        meta.settled_at = settled.then_some(stamp / 1000);
        meta.settled_override = settled.then_some(SettledOverride::Settled);
        store.upsert_meta(&meta).unwrap();
        store
            .append_event(
                id,
                stamp,
                &AgentEvent::ItemCompleted(ThreadItem {
                    id: "old-request".into(),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: "Earlier work".into(),
                        attachments: vec![],
                    },
                }),
            )
            .unwrap();
    }
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| state.settings.auto_settle_after_days = None);
    let command = |state: &TestEntity, cx: &mut TestAppContext, id, command| {
        through_pipe(
            state,
            cx,
            id,
            tcode_protocol::ClientPayload::Command(command),
        )
        .into_iter()
        .find_map(|message| match message {
            HostMessage::Ack { id: acked, result } if acked == id => Some(result),
            _ => None,
        })
        .unwrap()
        .map(|_| ())
        .map_err(|error| error.code)
    };

    command(
        &state,
        cx,
        1,
        Command::PinSession {
            session_id: "parked".into(),
            order_key: Some("n".into()),
        },
    )
    .unwrap();
    state.read(|state| {
        let parked = state.find_meta("parked").unwrap();
        assert!(!parked.is_settled());
        assert_eq!(parked.settled_override, Some(SettledOverride::Active));
        assert_eq!(parked.unsettled_at, None, "a pin is not a reopen");
        assert_eq!(parked.pin_order.as_deref(), Some("n"));
    });
    command(
        &state,
        cx,
        2,
        Command::PinSession {
            session_id: "idle".into(),
            order_key: None,
        },
    )
    .unwrap();
    assert_eq!(
        command(
            &state,
            cx,
            4,
            Command::ReorderPinned {
                session_id: "arranged".into(),
                order_key: "m".into(),
            },
        ),
        Err("not_pinned".into())
    );
    command(
        &state,
        cx,
        9,
        Command::ReorderPinned {
            session_id: "idle".into(),
            order_key: "p".into(),
        },
    )
    .unwrap();
    state.read(|state| {
        let idle = state.find_meta("idle").unwrap();
        assert_eq!(idle.updated_at, stamp / 1000, "arranging is not activity");
    });

    state.dispatch_command(
        cx,
        6,
        Command::PatchSettings {
            patch: SettingsPatch::AutoSettleAfterDays(Some(3.0)),
        },
    );
    cx.run_until(|state| {
        state
            .find_meta("arranged")
            .is_some_and(|meta| meta.is_settled())
    });
    state.read(|state| {
        let idle = state.find_meta("idle").unwrap();
        assert!(!idle.is_settled(), "a pinned thread is exempt");
    });

    command(
        &state,
        cx,
        7,
        Command::UnpinSession {
            session_id: "parked".into(),
        },
    )
    .unwrap();
    state.read(|state| {
        let parked = state.find_meta("parked").unwrap();
        assert_eq!((parked.pinned_at, parked.pin_order), (None, None));
        assert_eq!(
            parked.settled_override,
            Some(SettledOverride::Active),
            "unpinning keeps the thread active"
        );
        assert_eq!(
            parked.updated_at,
            stamp / 1000,
            "pinning and unpinning are not activity"
        );
    });
    command(
        &state,
        cx,
        8,
        Command::SettleSession {
            session_id: "idle".into(),
        },
    )
    .unwrap();
    state.read(|state| {
        let idle = state.find_meta("idle").unwrap();
        assert!(idle.is_settled());
        assert_eq!(idle.pinned_at, None, "settling unpins");
        for id in ["idle", "arranged"] {
            let meta = state.find_meta(id).unwrap();
            assert_eq!(meta.updated_at, stamp / 1000, "settling is not activity");
        }
    });

    command(
        &state,
        cx,
        10,
        Command::UnsettleSession {
            session_id: "idle".into(),
        },
    )
    .unwrap();
    command(
        &state,
        cx,
        11,
        Command::SetAutoSettle {
            session_id: "idle".into(),
            enabled: false,
        },
    )
    .unwrap();
    state.read(|state| {
        let idle = state.find_meta("idle").unwrap();
        assert!(!idle.is_settled() && idle.auto_settle_disabled_at.is_some());
        assert_eq!(
            idle.updated_at,
            stamp / 1000,
            "un-settling and the auto-settle toggle are not activity"
        );
    });
}

#[test]
fn an_old_profiles_activity_backfills_and_updates_the_index() {
    let store = TestStore::new("activity-backfill");
    std::fs::create_dir_all(store.root()).unwrap();
    // One more thread than a backfill batch holds, so the backfill goes on
    // past its first batch.
    let metas: Vec<_> = (0..17)
        .map(|index| {
            let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
            meta.id = format!("old-{index}");
            meta
        })
        .collect();
    std::fs::write(
        store.root().join("sessions.json"),
        serde_json::to_vec(&serde_json::json!({"projects":[], "sessions":metas})).unwrap(),
    )
    .unwrap();
    for meta in &metas {
        std::fs::write(store.root().join(format!("{}.jsonl", meta.id)),
            "{\"ts\":100,\"event\":{\"type\":\"provider_start_failed\",\"error\":\"cannot start\"}}\n").unwrap();
    }
    store
        .migrate(|_| {}, &std::sync::atomic::AtomicBool::new(false))
        .unwrap();
    let host =
        crate::pipe::spawn_host((*store).clone(), crate::pipe::HostServices::default()).unwrap();
    let link = host.link();
    let events = crate::pipe::p4b_tests::observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    })
    .unwrap();
    let mut activity = HashMap::new();
    loop {
        match next_host_event(&events).event {
            ServerEvent::IndexSnapshot(snapshot) => activity = snapshot.summary.activity,
            ServerEvent::IndexSummaryReplaced(summary) => activity.extend(summary.activity),
            ServerEvent::IndexActivitiesChanged { upserts, removed } => {
                activity.extend(upserts);
                for id in removed {
                    activity.remove(&id);
                }
            }
            _ => continue,
        }
        // A thread this old settles, and the index leaves a settled thread
        // to the page a client reads it by.
        let mut activity = activity.clone();
        activity.extend(settled_threads(&link).activity);
        if activity.len() == metas.len() && activity.values().all(|activity| activity.failed) {
            break;
        }
    }
    link.shutdown_blocking().unwrap();
    host.to_host.close();
    host.stopped.recv_blocking().unwrap();
    let restarted = crate::pipe::spawn_host(
        SessionStore::open_at(store.root().clone()).unwrap(),
        crate::pipe::HostServices::default(),
    )
    .unwrap();
    let link = restarted.link();
    let events = crate::pipe::p4b_tests::observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    })
    .unwrap();
    loop {
        if let ServerEvent::IndexSnapshot(snapshot) = next_host_event(&events).event {
            let mut activity = snapshot.summary.activity;
            activity.extend(settled_threads(&link).activity);
            assert_eq!(activity.len(), metas.len());
            assert!(activity.values().all(|activity| activity.failed));
            break;
        }
    }
    link.shutdown_blocking().unwrap();
    restarted.to_host.close();
    restarted.stopped.recv_blocking().unwrap();
}

#[test]
fn deleting_the_last_child_releases_its_leads_automatic_settlement() {
    let store = TestStore::new("deleted-child-settlement");
    // The child finished ten days before the host starts.
    let clock = test_clock();
    let stamp = clock.host_clock().unix_millis();
    clock.advance(Duration::from_secs(10 * 86_400));
    for (id, parent) in [("lead", None), ("worker", Some("lead"))] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_owned);
        store.upsert_meta(&meta).unwrap();
        store
            .append_event(
                id,
                stamp,
                &AgentEvent::TurnCompleted {
                    turn_id: "finished".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            )
            .unwrap();
    }
    let host = crate::pipe::spawn_host(
        (*store).clone(),
        crate::pipe::HostServices {
            clock: clock.host_clock(),
            ..crate::pipe::HostServices::default()
        },
    )
    .unwrap();
    let link = host.link();
    let events = crate::pipe::p4b_tests::observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    })
    .unwrap();
    loop {
        if let ServerEvent::IndexSnapshot(snapshot) = next_host_event(&events).event {
            assert!(children_hold(&snapshot.summary.activity["lead"]));
            assert!(
                !snapshot
                    .sessions
                    .iter()
                    .find(|meta| meta.id == "lead")
                    .unwrap()
                    .is_settled()
            );
            break;
        }
    }
    link.command_blocking(Command::DeleteSession {
        session_id: "worker".into(),
        remove_worktree: false,
    })
    .unwrap();
    loop {
        if let ServerEvent::IndexUpsertSession(meta) = next_host_event(&events).event
            && meta.id == "lead"
            && meta.is_settled()
        {
            assert_eq!(meta.settled_at, Some(stamp / 1000));
            break;
        }
    }
    link.shutdown_blocking().unwrap();
    host.to_host.close();
    host.stopped.recv_blocking().unwrap();
}

/// Settling a lead's children settles each of its own dispatched children
/// that finished, skips one still running, and touches no thread of another
/// lead, no native subagent and not the lead.
#[test]
fn settle_all_children_delivers_finished_results_and_skips_running_children() {
    use tcode_core::settlement::AgentDelivery;
    use tcode_protocol::{ChildSettlement, CommandResponse, SettleChildrenResult};
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("settle-all-children");
    for (id, parent, native) in [
        ("lead", None, false),
        ("first", Some("lead"), false),
        ("second", Some("lead"), false),
        ("running", Some("lead"), false),
        ("other", Some("another-lead"), false),
        ("native", Some("lead"), true),
    ] {
        stored_thread(
            &store,
            id,
            ProviderKind::Codex,
            &store.root().join(id),
            |meta| {
                meta.parent_session_id = parent.map(str::to_owned);
                meta.native_subagent = native.then(|| "native-item".into());
            },
        );
    }
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    first_turn(&state, cx, &providers, "running", 0);
    state.leave(cx, "running");
    let Ok(CommandResponse::ChildrenSettled(mut result)) = state.command(
        cx,
        Command::SettleChildren {
            session_id: "lead".into(),
        },
    ) else {
        panic!("the children are settled")
    };
    let entry = |id: &str, delivery| ChildSettlement {
        thread_id: id.into(),
        delivery,
    };
    result.settled.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
    assert_eq!(
        result,
        SettleChildrenResult {
            settled: vec![
                entry("first", AgentDelivery::Settled),
                entry("second", AgentDelivery::Settled),
            ],
            skipped: vec![entry("running", AgentDelivery::Running)],
        }
    );
    for id in ["first", "second"] {
        assert!(cx.seen().session(id).unwrap().is_settled());
        assert_eq!(
            cx.seen().activity(id).unwrap().agent.unwrap().delivery,
            AgentDelivery::Settled
        );
    }
    for id in ["lead", "running", "other", "native"] {
        assert!(!cx.seen().session(id).unwrap().is_settled(), "{id}");
    }
}
