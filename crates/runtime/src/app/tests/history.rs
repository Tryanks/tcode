use super::*;

#[test]
fn mux_clients_target_independent_drafts_and_receive_only_their_session_tail() {
    use crate::pipe::{HostServices, spawn_host};
    use tcode_client::HostLink;
    use tcode_protocol::{Subscription, Topic};

    let root = TestStore::new("p4a-two-clients");
    let first = scripted_provider(ProviderKind::ClaudeCode);
    let second = scripted_provider(ProviderKind::ClaudeCode);
    let first_commands = first.commands.clone();
    let second_commands = second.commands.clone();
    let provider_events = [first.events.clone(), second.events.clone()];
    let launcher = ProviderLauncher(Arc::new(move |kind, options| {
        let launcher = if options.cwd.ends_with("one") {
            first.launcher.clone()
        } else {
            second.launcher.clone()
        };
        Box::pin(async move { (launcher.0)(kind, options).await })
    }));
    let host = spawn_host(
        (*root).clone(),
        HostServices {
            provider_launcher: launcher,
            ..HostServices::default()
        },
    )
    .unwrap();
    let mux = tcode_traverse::HostMux::new(host.attachments.clone(), host.from_host.clone());
    let connect = || {
        let connection = mux.attach(
            tcode_protocol::Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let link = HostLink::new(connection.to_host, connection.from_host);
        smol::spawn({
            let link = link.clone();
            async move { link.pump().await }
        })
        .detach();
        link
    };
    let one = connect();
    let two = connect();
    let draft = |link: &HostLink, name: &str| {
        let cwd = root.root().join(name);
        std::fs::create_dir_all(&cwd).unwrap();
        let CommandResponse::SessionId(Some(id)) = link
            .command_blocking(Command::StartDraft {
                project_id: "shared-project".into(),
                cwd,
            })
            .unwrap()
        else {
            panic!("draft id missing")
        };
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionEvents {
                session_id: id.clone(),
            },
            after: None,
        })
        .unwrap();
        let window = link
            .events()
            .recv_blocking()
            .expect("the subscription is answered before the link closes");
        assert!(matches!(window.event, ServerEvent::SessionSnapshot { .. }));
        id
    };
    let id_one = draft(&one, "one");
    let id_two = draft(&two, "two");
    assert_ne!(id_one, id_two);
    for (link, id, text) in [
        (&one, &id_one, "from desktop"),
        (&two, &id_two, "from phone"),
    ] {
        link.command_blocking(Command::SendTurn {
            session_id: id.clone(),
            text: text.into(),
            attachment_paths: Vec::new(),
        })
        .unwrap();
    }
    let receive_turn = |commands: &smol::channel::Receiver<SessionCommand>, expected: &str| {
        let command = commands
            .recv_blocking()
            .expect("the provider receives the turn before its channel closes");
        let SessionCommand::SendTurn { text, .. } = command else {
            panic!("expected send")
        };
        assert_eq!(text, expected);
    };
    receive_turn(&first_commands, "from desktop");
    receive_turn(&second_commands, "from phone");
    for events in &provider_events {
        for text in ["first", "second", "third"] {
            events
                .try_send(AgentEvent::Warning {
                    message: text.into(),
                })
                .unwrap();
        }
    }
    // Each client is sent its thread's third record; what either was sent
    // went out before the acknowledgement of a command sent after that.
    let mut received = [Vec::new(), Vec::new()];
    for (link, received) in [&one, &two].into_iter().zip(&mut received) {
        let events = link.events();
        while received
            .iter()
            .filter(|event: &&EventEnvelope| matches!(event.event, ServerEvent::SessionEvent(_)))
            .count()
            < 3
        {
            received.push(events.recv_blocking().unwrap());
        }
    }
    for ((link, id), mut received) in [(&one, &id_one), (&two, &id_two)].into_iter().zip(received) {
        let events = link.events();
        // The app consumes events while commands await their FIFO acknowledgement.
        smol::block_on(smol::future::race(
            async {
                link.command(Command::ClearRelaunchMarker).await.unwrap();
            },
            async {
                loop {
                    received.push(events.recv().await.unwrap());
                }
            },
        ));
        while let Ok(event) = events.try_recv() {
            received.push(event);
        }
        let mut records = 0;
        for event in received {
            assert_eq!(
                event.topic,
                Topic::SessionEvents {
                    session_id: id.clone()
                }
            );
            records += usize::from(matches!(event.event, ServerEvent::SessionEvent(_)));
        }
        assert_eq!(records, 3);
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionEvents {
                session_id: id.clone(),
            },
            after: Some(1),
        })
        .unwrap();
        link.command_blocking(Command::ClearRelaunchMarker).unwrap();
        let snapshot = events.try_recv().unwrap();
        let ServerEvent::SessionSnapshot { from, records, .. } = snapshot.event else {
            panic!("expected tail")
        };
        assert_eq!(from, 1);
        assert_eq!(records.len(), 2);
        assert!(
            matches!(&records[0].event, AgentEvent::Warning { message } if message == "second")
        );
        assert!(events.try_recv().is_err());
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionEvents {
                session_id: id.clone(),
            },
            after: Some(99),
        })
        .unwrap();
        link.command_blocking(Command::ClearRelaunchMarker).unwrap();
        assert!(
            matches!(events.try_recv().unwrap().event, ServerEvent::SessionSnapshot { from: 0, records, .. } if records.len() == 3)
        );
    }
    assert!(
        one.events().try_recv().is_err(),
        "second client's subscription leaked a snapshot"
    );
    one.shutdown_blocking().unwrap();
    host.to_host.close();
    host.stopped.recv_blocking().unwrap();
}

#[test]
fn archived_revision_invalidates_sharing_facts_when_another_thread_or_draft_leaves() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("archived-worktree-sharing");
    let project = Project::from_root(store.root().join("project"));
    store.upsert_project(&project).unwrap();
    let mut owner = SessionMeta::new(ProviderKind::Codex, store.root().join("owner"), None);
    owner.id = "owner".into();
    owner.archived_at = Some(1);
    owner.worktree = Some(WorktreeInfo {
        root_project_path: project.root.clone(),
        base: "main".into(),
        branch: "tcode/owner".into(),
    });
    store.upsert_meta(&owner).unwrap();
    let mut fork = SessionMeta::new(ProviderKind::Codex, owner.cwd.join("nested"), None);
    fork.id = "fork".into();
    store.upsert_meta(&fork).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        let before = state.archived_sessions();
        assert!(before.worktree_shared.contains("owner"));
        state.delete_session("fork", false, cx).detach();
        let after = state.archived_sessions();
        assert!(after.revision > before.revision);
        assert!(!after.worktree_shared.contains("owner"));
        let draft = state
            .host
            .start_draft(project.id.clone(), owner.cwd.clone(), cx)
            .unwrap();
        let shared = state.archived_sessions();
        assert!(shared.revision > after.revision);
        assert!(shared.worktree_shared.contains("owner"));
        state.shutdown_active(&draft, cx);
        let unshared = state.archived_sessions();
        assert!(unshared.revision > shared.revision);
        assert!(!unshared.worktree_shared.contains("owner"));
    });
}

/// The status a client is sent carries the thread's usage and context window
/// however little of the log the client holds: one opening the thread after
/// many later records gets a window that starts after the usage.
#[test]
fn usage_projection_survives_a_partial_history_window() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("session-plan-usage-window");
    let id = "plan-usage";
    stored_thread(
        &store,
        id,
        ProviderKind::ClaudeCode,
        &store.root().join("thread"),
        |meta| {
            meta.model = Some("claude-sonnet-4-6".into());
            meta.option_selections = vec![OptionSelection {
                id: "contextWindow".into(),
                value: serde_json::json!(500_000),
            }];
        },
    );
    let (state, providers) = played_host(cx, &store);
    let provider = first_turn(&state, cx, &providers, id, 0);
    provider.play([
        AgentEvent::TokenUsage(agent::TokenUsage {
            context_window: Some(1_000_000),
            used_tokens: Some(1234),
            total_processed_tokens: Some(9876),
            freshness: agent::ContextFreshness::Current,
            ..Default::default()
        }),
        completed_turn("turn"),
    ]);
    state.send(cx, id, "again");
    provider.start_turn(cx, "second");
    provider.play((0..401).map(|index| AgentEvent::Warning {
        message: format!("later record {index}"),
    }));
    provider.fence(cx, id);
    state.leave(cx, id);
    state.open(cx, id);
    let partial = cx.seen().timeline(id);
    assert!(
        !cx.seen()
            .records(id)
            .iter()
            .any(|record| matches!(record.event, AgentEvent::TokenUsage(_))),
        "the window starts after the usage"
    );
    assert!(partial.usage.is_none());
    let status = cx.seen().status(id);
    let usage = status.usage.unwrap();
    assert_eq!(usage.used_tokens, Some(1234));
    assert_eq!(usage.total_processed_tokens, Some(9876));
    assert_eq!(usage.context_window, Some(1_000_000));
    assert_eq!(status.context_window, Some(500_000));
}

/// A stopped turn reads as stopping until the harness ends it.
#[test]
fn interrupt_reports_stopping_until_the_turn_completes() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("interrupt-stopping");
    stored_thread(
        &store,
        "thread",
        ProviderKind::ClaudeCode,
        &store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &store);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    assert!(!cx.seen().status("thread").stopping);
    state
        .command(
            cx,
            Command::Interrupt {
                session_id: "thread".into(),
            },
        )
        .unwrap();
    assert!(cx.seen().status("thread").stopping);
    assert!(matches!(
        provider.next_command(cx),
        SessionCommand::Interrupt
    ));
    provider.play([turn_completed("turn", TurnStatus::Interrupted)]);
    cx.run_until_seen(|view| has_record(view, "thread", is_turn_end));
    let status = cx.seen().status("thread");
    assert!(!status.stopping);
    assert!(!status.activity.turn_running);
}

/// The status says which turn runs and since when, and the requests open on
/// the running harness: a background subagent's approval stays open after
/// the turn that spawned it ends.
#[test]
fn status_carries_the_running_turn_while_in_flight_and_open_requests_while_live() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("status-running-turn");
    stored_thread(
        &store,
        "thread",
        ProviderKind::ClaudeCode,
        &store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &store);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    provider.play([completed_turn("turn")]);
    state.send(cx, "thread", "again");
    provider.start_turn(cx, "turn-2");
    provider.play([AgentEvent::UserInputRequested {
        request_id: "ask".into(),
        parent_item_id: None,
        questions: Vec::new(),
        delivery: agent::UserInputDelivery::Blocking,
    }]);
    provider.fence(cx, "thread");
    let started_at = cx.seen().timeline("thread").turns[1].start_ts;
    assert!(started_at.is_some());
    let status = cx.seen().status("thread");
    assert_eq!(
        status.running_turn,
        Some(tcode_core::session::RunningTurn {
            turn: 1,
            started_at
        })
    );
    assert_eq!(
        status
            .pending_user_input
            .as_ref()
            .map(|pending| pending.request_id.as_str()),
        Some("ask")
    );

    // A background subagent keeps running, and asking, after the turn that
    // spawned it ends.
    let approval = |id: &str| {
        AgentEvent::ApprovalRequested(agent::ApprovalRequest {
            id: id.into(),
            turn_id: None,
            parent_item_id: Some("toolu_agent".into()),
            kind: agent::ApprovalKind::ExecCommand {
                command: "cp a b".into(),
                cwd: None,
                description: None,
            },
            reason: None,
            blocked_path: None,
            options: Vec::new(),
        })
    };
    provider.play([
        approval("subagent-during-turn"),
        completed_turn("turn-2"),
        approval("subagent-between-turns"),
    ]);
    provider.fence(cx, "thread");
    let status = cx.seen().status("thread");
    assert_eq!(status.running_turn, None);
    assert_eq!(
        status
            .pending_approvals
            .iter()
            .map(|request| request.id.as_str())
            .collect::<Vec<_>>(),
        ["subagent-during-turn", "subagent-between-turns"]
    );
    assert!(status.activity.waiting_for_approval);
}

/// A client back at a thread asks for what followed its cursor. What fits
/// one reply continues the cursor; what does not is answered, in the same one
/// reply, with the tail a client opening the thread fresh gets, so the client
/// is at the end of the log after one round trip either way.
#[test]
fn a_cursor_is_answered_in_one_reply_that_reaches_the_end_of_the_log() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-cursor-reply");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    // Each of the three long records takes most of half a reply.
    let long = |index: usize| format!("{index}{}", "x".repeat(200 * 1024));
    let messages: Vec<String> = ["first".to_owned()]
        .into_iter()
        .chain((0..3).map(long))
        .chain(["last".to_owned()])
        .collect();
    let records = messages
        .iter()
        .enumerate()
        .map(|(ts, message)| StoredEvent {
            origin: None,
            author: None,
            ts: Some(ts as u64),
            event: AgentEvent::SessionModel {
                model: message.clone(),
            },
            elided: None,
        });
    state.update(cx, |state, _| {
        state
            .event_records
            .insert("away".into(), session_log(records));
    });
    let reply = |messages: &[HostMessage], id: u64| -> serde_json::Value {
        let event = messages
            .iter()
            .find(|message| {
                matches!(message, HostMessage::Event(EventEnvelope { request_id: Some(request), .. }) if *request == id)
            })
            .expect("a snapshot answers the subscription");
        serde_json::from_str(&tcode_protocol::encode_line(event).unwrap()).unwrap()
    };
    let record = |ts: usize| serde_json::json!({"ts": ts, "event": {"type": "session_model", "model": messages[ts]}});

    // The client held the first row; the three long records and the last
    // one do not fit a reply.
    let behind = reply(
        &through_pipe(&state, cx, 1, subscribe_events("away", Some(1))),
        1,
    );
    assert_eq!(
        behind,
        serde_json::json!({"type": "event", "content": {
            "request_id": 1,
            "topic": {"type": "session_events", "content": {"session_id": "away"}},
            "event": {"type": "session_snapshot", "content": {
                "from": 2, "end": 5,
                "records": [record(2), record(3), record(4)],
                "total": 5, "total_turns": 0, "truncated": true,
            }},
        }}),
    );

    // Held through the second long record, the rest fits.
    let near = reply(
        &through_pipe(&state, cx, 2, subscribe_events("away", Some(3))),
        2,
    );
    assert_eq!(
        near,
        serde_json::json!({"type": "event", "content": {
            "request_id": 2,
            "topic": {"type": "session_events", "content": {"session_id": "away"}},
            "event": {"type": "session_snapshot", "content": {
                "from": 3, "end": 5,
                "records": [record(3), record(4)],
                "total": 5, "total_turns": 0, "truncated": false,
            }},
        }}),
    );
}

#[test]
fn session_history_snapshot_pages_and_absolute_tail_cursors() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-pages");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        let records: Vec<StoredEvent> = (0..2000)
            .map(|index| StoredEvent {
                origin: None,
                author: None,
                ts: Some(index),
                event: AgentEvent::SessionModel {
                    model: format!("event {index}"),
                },
                elided: None,
            })
            .collect();
        state
            .event_records
            .insert("large".into(), session_log(records.clone()));
        let subscription = tcode_protocol::Subscription {
            known_index: None,
            topic: Topic::SessionEvents {
                session_id: "large".into(),
            },
            after: None,
        };
        let snapshot = events_reply(state, &subscription);
        let ServerEvent::SessionSnapshot {
            from,
            records: tail,
            total,
            truncated,
            ..
        } = snapshot.event
        else {
            panic!("snapshot")
        };
        assert_eq!((from, total, truncated), (1600, 2000, false));
        assert_eq!(tail, records[1600..]);
        let mut before = from;
        let mut loaded = tail;
        while before > 0 {
            let QueryResponse::SessionHistoryPage {
                records: page,
                from,
                end,
                truncated,
            } = state.event_records["large"].history_page(before, 200, None)
            else {
                panic!("page")
            };
            assert!(!truncated);
            assert_eq!(end, before);
            assert_eq!(from + page.len() as u64, before);
            loaded.splice(0..0, page);
            before = from;
        }
        assert_eq!(loaded, records);
        for after in [0, 17, 1800, 1999, 2000] {
            let snapshot = events_reply(
                state,
                &tcode_protocol::Subscription {
                    after: Some(after),
                    ..subscription.clone()
                },
            );
            let ServerEvent::SessionSnapshot {
                from,
                records: tail,
                ..
            } = snapshot.event
            else {
                panic!("tail")
            };
            assert_eq!(from, after);
            assert_eq!(tail, records[after as usize..]);
        }
    });
}

/// Streaming providers write hundreds of delta records per turn. A window cut
/// at a record count would start mid-turn, and the client's fold of that
/// window would keep changing shape as earlier records arrive; both the
/// initial snapshot and each page instead start where a turn starts, within
/// the same byte envelope.
#[test]
fn history_snapshot_and_pages_start_at_turn_boundaries() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-turn-pages");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        let mut records = Vec::new();
        let mut turn_starts = Vec::new();
        for turn in 0..5u64 {
            turn_starts.push(records.len());
            records.push(StoredEvent {
                origin: None,
                author: None,
                ts: Some(turn * 1000),
                event: AgentEvent::ItemCompleted(ThreadItem {
                    id: format!("user-{turn}"),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: format!("question {turn}"),
                        attachments: Vec::new(),
                    },
                }),
                elided: None,
            });
            records.push(StoredEvent {
                origin: None,
                author: None,
                ts: Some(turn * 1000 + 1),
                event: AgentEvent::TurnStarted {
                    turn_id: format!("turn-{turn}"),
                },
                elided: None,
            });
            for delta in 0..300u64 {
                records.push(StoredEvent {
                    origin: None,
                    author: None,
                    ts: Some(turn * 1000 + 2 + delta),
                    event: AgentEvent::Delta {
                        item_id: "pi-assistant-0:0".into(),
                        kind: agent::DeltaKind::AssistantText,
                        text: "word ".into(),
                    },
                    elided: None,
                });
            }
            records.push(StoredEvent {
                origin: None,
                author: None,
                ts: Some(turn * 1000 + 400),
                event: AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{turn}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
                elided: None,
            });
        }
        assert_eq!(turn_starts, [0, 303, 606, 909, 1212]);
        state
            .event_records
            .insert("streamed".into(), session_log(records.clone()));

        let snapshot = events_reply(
            state,
            &tcode_protocol::Subscription {
                known_index: None,
                topic: Topic::SessionEvents {
                    session_id: "streamed".into(),
                },
                after: None,
            },
        );
        let ServerEvent::SessionSnapshot { from, .. } = snapshot.event else {
            panic!("snapshot")
        };
        assert_eq!(
            from, 909,
            "the 400th-from-last record falls inside turn 3, which loads whole"
        );

        let QueryResponse::SessionHistoryPage {
            from: page_from,
            end,
            records: page,
            truncated,
        } = state.event_records["streamed"].history_page(from, 200, None)
        else {
            panic!("page")
        };
        assert_eq!(
            (page_from, end),
            (606, 909),
            "a 200-record page grows back to its turn start"
        );
        let AgentEvent::Delta { text, .. } = &page[2].event else {
            panic!("merged delta")
        };
        assert_eq!(
            (page.len(), text.as_str()),
            (4, "word ".repeat(300).as_str()),
            "a turn's consecutive deltas cross the wire as one"
        );
        assert_eq!(page[..2], records[606..608]);
        assert_eq!(page[3], records[908]);
        assert!(!truncated);
    });
}

/// A record appended to an opened session reaches the window a client
/// resubscribing after it asks for, and the pages before it.
#[test]
fn an_appended_record_reaches_the_window_and_the_pages_after_it() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-log-residency");
    open_stored_session(&store, "resident");
    let persisted = persist_streamed_turns(&store, "resident", 2).len() as u64;
    let (state, providers) = played_host(cx, &store);

    window_reply(
        &through_pipe(&state, cx, 1, subscribe_events("resident", None)),
        1,
    );
    // The user asks one more question, which the provider takes.
    state.send(cx, "resident", "one more question");
    let provider = providers.started(cx, 0);
    let (delivery_id, _) = provider.next_turn(cx);
    provider.play([AgentEvent::TurnAccepted { delivery_id }]);
    let asked = |event: &AgentEvent| {
        matches!(event, AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::UserMessage { text, .. }, ..
        }) if text == "one more question")
    };
    cx.run_until_seen(|view| has_record(view, "resident", asked));
    let window = window_reply(
        &through_pipe(&state, cx, 2, subscribe_events("resident", Some(persisted))),
        2,
    );
    assert_eq!((window.total, window.total_turns), (persisted + 1, 3));
    assert!(matches!(&window.records[..], [record] if asked(&record.event)));
    let page = tcode_protocol::ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage {
        session_id: "resident".into(),
        before: persisted + 1,
        limit: 1,
    });
    let QueryResponse::SessionHistoryPage { from, records, .. } =
        query_reply(&through_pipe(&state, cx, 3, page), 3)
    else {
        panic!("page")
    };
    assert_eq!((from, records.len()), (persisted, 1));
}

/// Opening or archiving one thread must cost that thread on the wire, not the
/// whole index or every setting; archived threads leave the index and are
/// read on demand.
#[test]
fn index_and_visit_changes_cross_the_wire_one_thread_at_a_time() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("index-deltas");
    let project = Project::from_root(store.root().join("project"));
    store.upsert_project(&project).unwrap();
    for id in ["kept", "archived"] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, project.root.clone(), None);
        meta.id = id.into();
        meta.project_id = Some(project.id.clone());
        store.upsert_meta(&meta).unwrap();
    }
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.deliver(
        cx,
        1000,
        tcode_protocol::ClientPayload::Subscribe(Subscription {
            topic: Topic::Index,
            after: None,
            known_index: None,
        }),
    );
    cx.run_until_parked();
    cx.drain_outgoing();

    state.dispatch_command(
        cx,
        1,
        Command::ArchiveSession {
            session_id: "archived".into(),
        },
    );
    cx.run_until_parked();
    let events: Vec<_> = cx
        .drain_outgoing()
        .into_iter()
        .filter_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                topic: Topic::Index,
                event,
                ..
            }) => Some(event),
            _ => None,
        })
        .collect();
    assert!(
        events.iter().any(|event| matches!(
            event,
            ServerEvent::IndexRemoveSession { session_id } if session_id == "archived"
        )),
        "{events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        ServerEvent::IndexSummaryReplaced(summary)
            if summary.archived_counts.get(&project.id) == Some(&1)
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ServerEvent::IndexSnapshot(_))),
        "a change never resends the whole index"
    );
    state.read(|state| {
        let index = state.index_snapshot();
        assert_eq!(
            index
                .sessions
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["kept"]
        );
        assert_eq!(
            state
                .archived_sessions()
                .sessions
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["archived"]
        );
    });

    let revision = state.read(|state| state.index_snapshot().summary.archived_revision);
    state.dispatch_command(
        cx,
        2,
        Command::RenameSession {
            session_id: "archived".into(),
            title: "Archived rename".into(),
        },
    );
    cx.run_until_parked();
    let summary_revision = cx
        .drain_outgoing()
        .into_iter()
        .find_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                event: ServerEvent::IndexSummaryReplaced(summary),
                ..
            }) => Some(summary.archived_revision),
            _ => None,
        })
        .expect("archived rename must invalidate the held list");
    assert!(summary_revision > revision);
    state.update(cx, |state, cx| {
        crate::pipe::handle_client_message(
            state,
            cx,
            tcode_protocol::ClientMessage {
                principal: None,
                id: 3,
                key: None,
                payload: tcode_protocol::ClientPayload::Query(
                    tcode_protocol::Query::ArchivedSessions,
                ),
            },
            tcode_client::attachment::AttachmentPolicy::LOCAL,
        )
    });
    let (archived, _) = cx.run_until_sent(|message| match message {
        HostMessage::QueryResult {
            id: 3,
            result: Ok(QueryResponse::ArchivedSessions(archived)),
        } => Some(archived.clone()),
        _ => None,
    });
    assert_eq!(archived.revision, summary_revision);
    assert_eq!(archived.sessions[0].title, "Archived rename");

    state.dispatch_command(
        cx,
        4,
        Command::MarkSessionRead {
            session_id: "kept".into(),
            through: 1,
        },
    );
    cx.run_until_parked();
    let settings: Vec<_> = cx
        .drain_outgoing()
        .into_iter()
        .filter_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                topic: Topic::Settings,
                event,
                ..
            }) => Some(event),
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            settings.as_slice(),
            [ServerEvent::LastVisitedChanged(visits)] if visits.keys().eq(["kept"])
        ),
        "{settings:?}"
    );
}

/// A tool's screenshot or a long build log costs its preview when a thread
/// opens; the rest stays on the host until the reader asks for it.
#[test]
fn history_sends_output_previews_and_reads_whole_outputs_on_request() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-output-previews");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let preview = tcode_protocol::OUTPUT_PREVIEW_BYTES;
    let tool_output = format!("{}{}", "a".repeat(preview), "é".repeat(50_000));
    let command_output = format!("{}{}", "b".repeat(100_000), "z".repeat(preview));
    let image_content = serde_json::json!([
        {"type":"text", "text":"Dimensions: 1 × 1"},
        {"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":"AQID"}}
    ]);
    let image_output = image_content.to_string();
    let mut image_record = tool_call("image", image_output.clone());
    if let AgentEvent::ItemCompleted(ThreadItem {
        content: ItemContent::ToolCall { image_reads, .. },
        ..
    }) = &mut image_record.event
    {
        *image_reads = agent::ToolOutput::from_content(&image_content).images;
    }
    let records = vec![
        tool_call("tool", tool_output.clone()),
        StoredEvent {
            origin: None,
            author: None,
            ts: Some(2),
            event: AgentEvent::ItemCompleted(ThreadItem {
                id: "command".into(),
                parent_item_id: None,
                content: ItemContent::CommandExecution {
                    command: "make".into(),
                    input: Vec::new(),
                    output: command_output.clone(),
                    exit_code: Some(0),
                    status: ItemStatus::Completed,
                },
            }),
            elided: None,
        },
        tool_call("small", "ok".into()),
        image_record,
    ];
    state.update(cx, |state, _| {
        state
            .event_records
            .insert("outputs".into(), session_log(records.clone()));
        let snapshot = events_reply(
            state,
            &tcode_protocol::Subscription {
                known_index: None,
                topic: Topic::SessionEvents {
                    session_id: "outputs".into(),
                },
                after: None,
            },
        );
        let ServerEvent::SessionSnapshot { records: sent, .. } = snapshot.event else {
            panic!("snapshot")
        };
        let AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::ToolCall {
                output: Some(head), ..
            },
            ..
        }) = &sent[0].event
        else {
            panic!("tool call")
        };
        assert_eq!(head.as_str(), "a".repeat(preview));
        assert_eq!(sent[0].elided, Some(tool_output.len() as u64));
        let AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::CommandExecution { output: tail, .. },
            ..
        }) = &sent[1].event
        else {
            panic!("command")
        };
        assert_eq!(
            tail.as_str(),
            "z".repeat(preview),
            "a command keeps its end"
        );
        assert_eq!(sent[1].elided, Some(command_output.len() as u64));
        assert_eq!(sent[2], records[2], "a small output crosses whole");
        let AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::ToolCall { image_reads, .. },
            ..
        }) = &sent[3].event
        else {
            panic!("image tool")
        };
        assert!(!serde_json::to_string(&sent[3]).unwrap().contains("AQID"));
        assert_eq!(
            state.event_records["outputs"]
                .item_output("outputs", "image")
                .unwrap(),
            QueryResponse::ItemOutput(image_output.clone())
        );
        assert_eq!(image_reads.len(), 1);
        assert_eq!(image_reads[0].media_type, "image/png");
        assert!(
            image_reads[0].data_base64.is_empty(),
            "pixels stay on the host even with a short text output"
        );
        assert_eq!(
            state.event_records["outputs"]
                .item_image("image", 0, None)
                .unwrap()
                .read()
                .unwrap(),
            QueryResponse::FileBytes(vec![1, 2, 3])
        );
        assert!(
            state.event_records["outputs"]
                .item_image("image", 1, None)
                .is_err()
        );

        let timeline = Timeline::fold_events(sent);
        assert_eq!(
            timeline.elided_outputs,
            HashMap::from([
                ("tool".to_string(), tool_output.len() as u64),
                ("command".to_string(), command_output.len() as u64),
                ("image".to_string(), image_output.len() as u64),
            ])
        );
        assert_eq!(
            state.event_records["outputs"]
                .item_output("outputs", "tool")
                .unwrap(),
            QueryResponse::ItemOutput(tool_output.clone())
        );
        assert_eq!(
            state.event_records["outputs"]
                .item_output("outputs", "command")
                .unwrap(),
            QueryResponse::ItemOutput(command_output.clone())
        );
        assert_eq!(
            state.event_records["outputs"]
                .item_output("outputs", "missing")
                .unwrap_err()
                .code,
            "unknown_item_output"
        );
    });
}

/// A whole output is sent when one line of the asking client's connection
/// holds its reply: an in-process client takes any size, and a client over
/// the network is told by name when the escaped reply is longer than its
/// connection's line, which would otherwise close the connection.
#[test]
fn a_whole_output_is_sent_when_the_clients_line_holds_it() {
    use tcode_client::attachment::AttachmentPolicy;
    use tcode_protocol::{ClientMessage, ClientPayload, MAX_LINE_BYTES, Query};

    let cx = &mut TestAppContext::default();
    let store = TestStore::new("whole-output-line");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    // Longer than half a network line, and short of one.
    let long = "a".repeat(MAX_LINE_BYTES / 2 + 1);
    // A quarter of a network line, each byte of which takes six in the reply.
    let escapes = "\u{1b}".repeat(MAX_LINE_BYTES / 4);
    state.update(cx, |state, _| {
        state.event_records.insert(
            "outputs".into(),
            session_log([
                tool_call("long", long.clone()),
                tool_call("escapes", escapes.clone()),
            ]),
        );
    });
    let mut read = |id: u64, item_id: &str, policy: AttachmentPolicy| {
        state.update(cx, |state, host_cx| {
            crate::pipe::handle_client_message(
                state,
                host_cx,
                ClientMessage {
                    principal: None,
                    key: None,
                    id,
                    payload: ClientPayload::Query(Query::ReadItemOutput {
                        session_id: "outputs".into(),
                        item_id: item_id.into(),
                    }),
                },
                policy,
            )
        });
        answered(cx, id)
            .into_iter()
            .find_map(|message| match message {
                HostMessage::QueryResult { id: reply, result } if reply == id => Some(result),
                _ => None,
            })
            .expect("the read is answered")
    };

    assert_eq!(
        read(2, "long", AttachmentPolicy::network()),
        Ok(QueryResponse::ItemOutput(long))
    );
    assert_eq!(
        read(3, "escapes", AttachmentPolicy::LOCAL),
        Ok(QueryResponse::ItemOutput(escapes))
    );
    assert_eq!(
        read(4, "escapes", AttachmentPolicy::network())
            .unwrap_err()
            .code,
        "item_output_too_large"
    );
}

/// Every update of a turn's changes carries the whole diff set; only the last
/// one decides what the turn shows, so the earlier ones cross without diffs.
#[test]
fn superseded_turn_changes_cross_without_diffs() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-turn-changes");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let changes = |diff: &str| {
        agent::file_changes_from_unified_diff(&format!(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-{diff}\n+{diff}!\n"
        ))
        .changes
    };
    let update = |diff: &str| StoredEvent {
        origin: None,
        author: None,
        ts: Some(1),
        event: AgentEvent::TurnChangesUpdated {
            turn_id: "turn".into(),
            changes: changes(diff),
            completeness: agent::ChangeCompleteness::Exact,
        },
        elided: None,
    };
    let records = vec![
        StoredEvent {
            origin: None,
            author: None,
            ts: Some(0),
            event: AgentEvent::TurnStarted {
                turn_id: "turn".into(),
            },
            elided: None,
        },
        update("first"),
        tool_call("between", "ok".into()),
        update("last"),
    ];
    state.update(cx, |state, _| {
        state
            .event_records
            .insert("changes".into(), session_log(records.clone()));
        let QueryResponse::SessionHistoryPage { records: sent, .. } =
            state.event_records["changes"].history_page(4, 200, None)
        else {
            panic!("page")
        };
        let AgentEvent::TurnChangesUpdated { changes, .. } = &sent[1].event else {
            panic!("first update")
        };
        assert!(changes.iter().all(|change| change.diff.is_none()));
        assert_eq!(sent[3], records[3]);
        assert_eq!(Timeline::fold_events(sent), Timeline::fold_events(records));
    });
}

/// A snapshot of a turn's changes appended to a thread a client holds drops
/// the diffs of the ones it supersedes from the store, at their own rows even
/// where a blank row puts rows and records out of step, and what the client
/// is sent folds as the store does. A log with a row that does not decode
/// keeps its stored rows: its fold may not name the snapshots another
/// build's would.
#[test]
fn an_appended_snapshot_drops_the_superseded_ones_diffs_in_the_same_commit() {
    for (label, gap, rewritten) in [
        ("blank", "\n", true),
        ("undecodable", "{not valid json}\n", false),
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("append-supersedes");
        stored_thread(
            &store,
            "thread",
            ProviderKind::Codex,
            &store.root().join("thread"),
            |_| {},
        );
        store
            .apply(&[tcode_services::store::Mutation::replace_event_log(
                "thread",
                gap.as_bytes().to_vec(),
            )])
            .unwrap();
        let (state, providers) = played_host(cx, &store);
        // The host indexes the stored log's turns as it starts.
        cx.run_until(|_| store.turn_index("thread").unwrap().is_some());
        let provider = first_turn(&state, cx, &providers, "thread", 0);
        let snapshot = |diff: &str| AgentEvent::TurnChangesUpdated {
            turn_id: "turn".into(),
            changes: vec![agent::FileChange {
                path: "f".into(),
                kind: agent::FileChangeKind::Modify,
                diff: Some(diff.into()),
            }],
            completeness: agent::ChangeCompleteness::Exact,
        };
        provider.play(["-a\n+b\n", "-a\n+c\n", "-a\n+d\n"].map(snapshot));
        provider.fence(cx, "thread");
        cx.run_until_parked();

        let diffs: Vec<bool> = store
            .read_events("thread")
            .unwrap()
            .iter()
            .filter_map(|record| match &record.event {
                AgentEvent::TurnChangesUpdated { changes, .. } => Some(changes[0].diff.is_some()),
                _ => None,
            })
            .collect();
        assert_eq!(
            diffs,
            if rewritten {
                [false, false, true]
            } else {
                [true, true, true]
            },
            "{label}: the stored snapshots"
        );
        assert!(
            String::from_utf8(store.read_event_log("thread").unwrap())
                .unwrap()
                .starts_with(gap),
            "{label}: the rows before stay"
        );
        assert_eq!(
            cx.seen().timeline("thread"),
            Timeline::fold_events(store.read_events("thread").unwrap()),
            "{label}"
        );
    }
}

/// The startup pass drops the superseded diffs of every thread stored before
/// appends did, an open one included, through the store writer.
#[test]
fn the_storage_pass_drops_superseded_diffs_of_every_stored_thread() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("diff-pass");
    let started = "{\"ts\":1,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"t\"}}\n";
    let completed = "{\"ts\":4,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"t\",\"status\":\"completed\",\"usage\":null}}\n";
    let log = format!(
        "{started}{}{}{completed}",
        stored_snapshot(2, "t", "-a\n+b\n"),
        stored_snapshot(3, "t", "-a\n+c\n"),
    );
    let passed = format!(
        "{started}{}{}{completed}",
        stored_snapshot(2, "t", ""),
        stored_snapshot(3, "t", "-a\n+c\n"),
    );
    for id in ["cold", "open"] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
        meta.id = id.into();
        store
            .apply(&[
                tcode_services::store::Mutation::upsert_meta(meta),
                tcode_services::store::Mutation::replace_event_log(id, log.clone().into_bytes()),
            ])
            .unwrap();
    }
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        state.select_session("open", cx);
        state.start_storage_pass(cx);
    });
    cx.run_until(|state| {
        state.storage_pass.is_none() && state.store.threads_to_compact().unwrap().is_empty()
    });
    for id in ["cold", "open"] {
        assert_eq!(
            String::from_utf8(store.read_event_log(id).unwrap()).unwrap(),
            passed,
            "{id}"
        );
    }
    drop(state);
    // A restart finds every thread passed and leaves each as it was.
    let cx = &mut TestAppContext::default();
    let restarted = cx.new_entity(TestClientState::new((*store).clone()));
    assert!(store.threads_to_compact().unwrap().is_empty());
    restarted.update(cx, |state, cx| state.start_storage_pass(cx));
    cx.run_until(|state| state.storage_pass.is_none());
    for id in ["cold", "open"] {
        assert_eq!(
            String::from_utf8(store.read_event_log(id).unwrap()).unwrap(),
            passed,
            "{id}"
        );
    }
}

/// A full device subscribed to the storage migration hears nothing while no
/// move runs; while the startup pass moves an older build's threads it hears
/// how far the move has come, as does one that subscribes then; once every
/// thread has moved it hears that none runs.
#[test]
fn clients_hear_of_the_move_of_an_older_builds_threads_only_while_it_runs() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("storage-migration");
    // An older build's index and log, which a start migrates into an older
    // build's database before the host opens it.
    let mut log = String::new();
    for turn in 0..3 {
        log.push_str(&format!(
            "{{\"ts\":{turn},\"event\":{{\"type\":\"turn_started\",\"turn_id\":\"t{turn}\"}}}}\n"
        ));
        log.push_str(&format!(
            "{{\"ts\":{turn},\"event\":{{\"type\":\"turn_completed\",\"turn_id\":\"t{turn}\",\"status\":\"completed\",\"usage\":null}}}}\n"
        ));
    }
    std::fs::write(store.root().join("older.jsonl"), log).unwrap();
    std::fs::write(
        store.root().join("sessions.json"),
        serde_json::json!({
            "projects": [],
            "sessions": [{
                "id": "older", "title": "Older", "provider": "codex",
                "cwd": "/work", "created_at": 1, "updated_at": 2
            }]
        })
        .to_string(),
    )
    .unwrap();
    assert!(store.needs_migration().unwrap());
    store
        .migrate(|_| {}, &std::sync::atomic::AtomicBool::new(false))
        .unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let migration = |message: &HostMessage| match message {
        HostMessage::Event(EventEnvelope {
            request_id,
            topic: Topic::StorageMigration,
            event: ServerEvent::StorageMigrationReplaced(migration),
        }) => Some((*request_id, migration.clone())),
        _ => None,
    };
    let subscription = tcode_protocol::ClientPayload::Subscribe(Subscription {
        topic: Topic::StorageMigration,
        after: None,
        known_index: None,
    });

    state.deliver(cx, 1, subscription.clone());
    let (reply, _) = cx.run_until_sent(migration);
    assert_eq!(reply, (Some(1), None), "no move runs before the pass");

    state.update(cx, |state, cx| state.start_storage_pass(cx));
    // One host turn at a time: the pass then waits on its first write while
    // a second client subscribes.
    let pushed = loop {
        cx.run_next();
        if let Some(pushed) = cx.drain_outgoing().iter().find_map(migration) {
            break pushed;
        }
    };
    let (None, Some(running)) = pushed else {
        panic!("the move was not pushed while it runs: {pushed:?}");
    };
    assert!(
        running.moved < running.total && !running.waiting_for_space,
        "{running:?}"
    );
    state.deliver(cx, 2, subscription.clone());
    assert_eq!(
        cx.drain_outgoing().iter().find_map(migration),
        Some((Some(2), Some(running))),
        "a client subscribing now hears the move"
    );

    let (finished, _) = cx.run_until_sent(|message| {
        migration(message)
            .filter(|(request_id, migration)| request_id.is_none() && migration.is_none())
    });
    assert_eq!(finished, (None, None));
    cx.run_until(|state| state.storage_pass.is_none());
    assert_eq!(store.move_progress().unwrap(), None, "every thread moved");
    state.deliver(cx, 3, subscription);
    let (reply, _) = cx.run_until_sent(migration);
    assert_eq!(reply, (Some(3), None));
}

/// A completion appended to a thread a client holds drops from the store the
/// deltas it repeats: the one that created the entry stays with its text
/// emptied, and a stream the completion only abbreviates stays whole. A log
/// an earlier host wrote whole keeps its deltas until the startup pass,
/// which leaves the same records. Each folds to the same thread.
#[test]
fn a_completion_drops_the_deltas_it_repeats_in_the_same_commit() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("completion-supersedes");
    for id in ["held", "cold"] {
        stored_thread(
            &store,
            id,
            ProviderKind::Codex,
            &store.root().join(id),
            |_| {},
        );
    }
    let delta = |id: &str, kind, text: &str| AgentEvent::Delta {
        item_id: id.into(),
        kind,
        text: text.into(),
    };
    let completed = |id: &str, content| {
        AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        })
    };
    let events = [
        delta("a", agent::DeltaKind::AssistantText, "Hel"),
        delta("a", agent::DeltaKind::AssistantText, "lo"),
        completed(
            "a",
            ItemContent::AssistantMessage {
                text: "Hello".into(),
                summarized: false,
            },
        ),
        delta(
            "c",
            agent::DeltaKind::CommandOutput,
            "x".repeat(40).as_str(),
        ),
        completed(
            "c",
            ItemContent::CommandExecution {
                command: "ls".into(),
                input: Vec::new(),
                output: "xxxx… 36 bytes omitted".into(),
                exit_code: Some(0),
                status: agent::ItemStatus::Completed,
            },
        ),
        completed_turn("t"),
    ];
    // An earlier host wrote the cold thread's turn whole.
    let started = AgentEvent::TurnStarted {
        turn_id: "t".into(),
    };
    for (ts, event) in std::iter::once(&started).chain(&events).enumerate() {
        store.append_event("cold", ts as u64, event).unwrap();
    }
    assert!(
        store
            .threads_to_compact()
            .unwrap()
            .contains(&"cold".to_owned())
    );

    let (state, providers) = played_host(cx, &store);
    let provider = first_turn(&state, cx, &providers, "held", 0);
    provider.play(events.clone());
    cx.run_until_seen(|view| has_record(view, "held", is_turn_end));
    cx.run_until_parked();
    let deltas = |id: &str| {
        store
            .read_events(id)
            .unwrap()
            .into_iter()
            .filter_map(|record| match record.event {
                AgentEvent::Delta { item_id, text, .. } => Some((item_id, text)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let kept = vec![
        ("a".to_string(), String::new()),
        ("c".to_string(), "x".repeat(40)),
    ];
    assert_eq!(deltas("held"), kept);
    assert_eq!(
        deltas("cold").len(),
        3,
        "a log nobody wrote this way keeps its deltas"
    );
    assert_eq!(
        cx.seen().timeline("held"),
        Timeline::fold_events(store.read_events("held").unwrap())
    );

    // The pass a host makes as it starts.
    state.update(cx, |state, cx| state.start_storage_pass(cx));
    cx.run_until(|_| store.threads_to_compact().unwrap().is_empty());
    assert_eq!(deltas("cold"), kept);
    let fold = |id: &str| {
        Timeline::fold_events(store.read_events(id).unwrap())
            .entries
            .into_iter()
            .map(|entry| entry.content.clone())
            .filter(|content| {
                matches!(
                    content,
                    EntryContent::Item(ItemContent::AssistantMessage { .. })
                        | EntryContent::Item(ItemContent::CommandExecution { .. })
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(fold("cold"), fold("held"));
}

/// A thread opens with about half a megabyte of history however long its
/// records are, a single record larger than that still arrives alone, and one
/// larger than the client's line arrives as its size and the row to read it
/// from.
#[test]
fn history_windows_are_byte_budgeted() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-window-bytes");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let records: Vec<StoredEvent> = (0..200)
        .map(|index| StoredEvent {
            origin: None,
            author: None,
            ts: Some(index),
            event: AgentEvent::Warning {
                message: "w".repeat(10 * 1024),
            },
            elided: None,
        })
        .collect();
    state.update(cx, |state, _| {
        state
            .event_records
            .insert("wide".into(), session_log(records.clone()));
        let mut snapshot = events_reply(
            state,
            &tcode_protocol::Subscription {
                known_index: None,
                topic: Topic::SessionEvents {
                    session_id: "wide".into(),
                },
                after: None,
            },
        );
        snapshot.request_id = Some(u64::MAX);
        let line = tcode_protocol::encode_line(&HostMessage::Event(snapshot.clone())).unwrap();
        assert!(line.len() <= tcode_protocol::SESSION_WINDOW_BYTES);
        let ServerEvent::SessionSnapshot {
            from,
            end,
            records: sent,
            truncated,
            ..
        } = snapshot.event
        else {
            panic!("snapshot")
        };
        assert!(truncated);
        assert_eq!(end, 200);
        assert!(sent.len() > 40, "the budget is filled, not undershot");
        assert_eq!(sent, records[from as usize..]);

        let mut huge = records;
        huge[199].event = AgentEvent::Warning {
            message: "w".repeat(2 * tcode_protocol::SESSION_WINDOW_BYTES),
        };
        state
            .event_records
            .insert("wide".into(), session_log(huge.clone()));
        let QueryResponse::SessionHistoryPage {
            from,
            records: sent,
            ..
        } = state.event_records["wide"].history_page(
            200,
            200,
            Some(tcode_protocol::MAX_LINE_BYTES),
        )
        else {
            panic!("page")
        };
        assert_eq!((from, sent.len()), (199, 1));

        // A record whose reply is longer than the connection's line is sent
        // as its size, and the records around it as they are.
        let huge_len = serde_json::to_vec(&state.event_records["wide"].records()[199])
            .unwrap()
            .len() as u64;
        let QueryResponse::SessionHistoryPage {
            from,
            records: sent,
            ..
        } = state.event_records["wide"].history_page(
            200,
            200,
            Some(2 * tcode_protocol::SESSION_WINDOW_BYTES),
        )
        else {
            panic!("page")
        };
        assert!(sent.len() > 40, "the rest of the page is sent");
        assert_eq!(
            sent.last().unwrap(),
            &StoredEvent {
                origin: None,
                author: None,
                ts: Some(199),
                event: AgentEvent::RecordWithheld {
                    bytes: huge_len,
                    row: Some(199),
                },
                elided: None,
            }
        );
        assert_eq!(sent[..sent.len() - 1], huge[from as usize..199]);
    });
}

/// A streamed delta that waits for company still reaches the database with
/// nothing written after it, and one still waiting when the host closes the
/// queue is committed before the writer stops.
#[test]
fn store_writer_commits_a_waiting_delta_on_its_own_and_at_shutdown() {
    let store = TestStore::new("tcode-delta-window");
    let (failures, _failures) = smol::channel::unbounded();
    let clock = test_clock();
    let writer = StoreWriter::new(
        (*store).clone(),
        SettingsStore::new(store.root().clone()),
        failures,
        clock.host_clock(),
    );
    let (writes, queue) = smol::channel::unbounded();
    let writer = std::thread::spawn(move || writer.run(queue));
    let delta = |text: &str| StoreWrite::AppendEvent {
        id: "streaming".into(),
        ts: 1,
        author: None,
        origin: None,
        event: Box::new(AgentEvent::Delta {
            item_id: "answer".into(),
            kind: agent::DeltaKind::AssistantText,
            text: text.into(),
        }),
        streams: None,
        metas: Vec::new(),
    };
    let texts = || -> Vec<String> {
        store
            .read_events("streaming")
            .unwrap()
            .into_iter()
            .map(|stored| match stored.event {
                AgentEvent::Delta { text, .. } => text,
                other => panic!("unexpected record {other:?}"),
            })
            .collect()
    };

    writes.send_blocking(delta("first")).unwrap();
    smol::block_on(clock.advance_to_next_timer());
    // The writer opens a window for the next delta only once the last one is
    // committed, so the window it opens is the moment to read the store.
    writes.send_blocking(delta(" second")).unwrap();
    smol::block_on(clock.next_timer());
    assert_eq!(texts(), ["first"]);

    drop(writes);
    writer.join().unwrap();
    assert_eq!(texts(), ["first", " second"]);
}

#[test]
fn clients_opening_a_cold_thread_through_the_mux_get_each_record_once() {
    use crate::pipe::{HostServices, spawn_host};
    use tcode_client::HostLink;

    let root = TestStore::new("cold-open-mux");
    open_stored_session(&root, "cold");
    for index in 0..3 {
        root.append_event(
            "cold",
            index,
            &AgentEvent::Warning {
                message: format!("persisted {index}"),
            },
        )
        .unwrap();
    }
    let persisted = root.read_events("cold").unwrap();
    let provider = scripted_provider(ProviderKind::ClaudeCode);
    let host = spawn_host(
        (*root).clone(),
        HostServices {
            provider_launcher: provider.launcher.clone(),
            ..HostServices::default()
        },
    )
    .unwrap();
    let mux = tcode_traverse::HostMux::new(host.attachments.clone(), host.from_host.clone());
    let clients: Vec<HostLink> = (0..2)
        .map(|_| {
            let connection = mux.attach(
                tcode_protocol::Principal::Full,
                tcode_client::attachment::AttachmentPolicy::network(),
            );
            let link = HostLink::new(connection.to_host, connection.from_host);
            smol::spawn({
                let link = link.clone();
                async move { link.pump().await }
            })
            .detach();
            link
        })
        .collect();
    // One client shows the thread's status and sends to it; its log has not
    // reached any client.
    clients[0]
        .subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionStatus {
                session_id: "cold".into(),
            },
            after: None,
        })
        .unwrap();
    clients[0]
        .command_blocking(Command::SendTurn {
            session_id: "cold".into(),
            text: "go".into(),
            attachment_paths: Vec::new(),
        })
        .unwrap();
    assert!(matches!(
        crate::app::next_command_blocking(&provider.commands),
        Ok(SessionCommand::SendTurn { .. })
    ));
    for link in &clients {
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionEvents {
                session_id: "cold".into(),
            },
            after: None,
        })
        .unwrap();
    }
    provider
        .events
        .try_send(AgentEvent::Warning {
            message: "first live".into(),
        })
        .unwrap();
    for link in &clients {
        let mut received = Vec::new();
        let events = link.events();
        smol::block_on(async {
            let mut has_window = false;
            while received.len() < persisted.len() + 1 {
                let event = events
                    .recv()
                    .await
                    .expect("the host delivers the thread before the link closes")
                    .event;
                if let Some(window) = window_of(&event) {
                    assert!(!has_window, "one baseline per client");
                    assert_eq!(window.from, 0);
                    received.extend(window.records);
                    has_window = true;
                } else if let ServerEvent::SessionEvent(record) = event {
                    assert!(has_window, "baseline precedes live records");
                    received.push(record);
                } else if !matches!(event, ServerEvent::SessionStatusReplaced(_)) {
                    panic!("unexpected {event:?}");
                }
            }
            // A FIFO reply proves all preceding records have reached the pump.
            link.query(tcode_protocol::Query::Ping).await.unwrap();
            assert!(
                std::iter::from_fn(|| events.try_recv().ok()).all(|event| !matches!(
                    event.event,
                    ServerEvent::SessionEvent(_) | ServerEvent::SessionSnapshot { .. }
                )),
                "no duplicate records after the tail"
            );
        });
        assert_eq!(&received[..persisted.len()], persisted.as_slice());
        assert!(
            matches!(&received.last().unwrap().event, AgentEvent::Warning { message } if message == "first live")
        );
    }
    clients[0].shutdown_blocking().unwrap();
    for link in clients {
        link.close();
    }
    host.to_host.close();
    host.stopped.recv_blocking().unwrap();
}

/// A subscriber that leaves a cold thread before its log is read gets no
/// window for it.
#[test]
fn a_cold_thread_left_before_its_log_is_read_sends_no_window() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("cold-left");
    open_stored_session(&store, "left");
    persist_streamed_turns(&store, "left", 2);
    let state = cx.new_entity(TestClientState::new((*store).clone()));

    send_to_host(&state, cx, 1, subscribe_events("left", None));
    send_to_host(&state, cx, 2, unsubscribe_events("left"));
    state.read(|state| assert!(state.log_hydrations.contains_key("left")));
    let mut messages = answered(cx, 1);
    // The whole log's read can finish after the subscription is answered.
    cx.run_until(|state| state.log_hydrations.is_empty());
    messages.extend(cx.drain_outgoing());

    assert!(
        messages
            .iter()
            .any(|message| matches!(message, HostMessage::Ack { id: 2, .. }))
    );
    assert!(
        !messages.iter().any(|message| matches!(
            message,
            HostMessage::Event(EventEnvelope {
                topic: Topic::SessionEvents { .. },
                ..
            })
        )),
        "nothing of the thread reaches the client that left it"
    );
}

/// History pages and whole outputs of a thread no client holds open are
/// answered from its whole log, read off the mailbox.
#[test]
fn pages_and_outputs_of_a_thread_nobody_opened_are_read_from_its_log() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("cold-queries");
    open_stored_session(&store, "cold");
    let mut records = persist_streamed_turns(&store, "cold", 2);
    let output = "o".repeat(2 * tcode_protocol::OUTPUT_PREVIEW_BYTES);
    let tool = tool_call("tool", output.clone());
    store.append_event("cold", 5000, &tool.event).unwrap();
    records.push(tool);
    let total = records.len() as u64;
    let state = cx.new_entity(TestClientState::new((*store).clone()));

    let QueryResponse::SessionHistoryPage {
        end, records: page, ..
    } = query_reply(&through_pipe(&state, cx, 1, history_page("cold", total)), 1)
    else {
        panic!("page")
    };
    assert_eq!(end, total);
    assert!(
        matches!(&page.last().unwrap().event, AgentEvent::ItemCompleted(item) if item.id == "tool")
    );
    let read_output = tcode_protocol::ClientPayload::Query(tcode_protocol::Query::ReadItemOutput {
        session_id: "cold".into(),
        item_id: "tool".into(),
    });
    assert_eq!(
        query_reply(&through_pipe(&state, cx, 2, read_output), 2),
        QueryResponse::ItemOutput(output)
    );
    // The bounded read answers first; the whole log's read finishes after.
    cx.run_until(|state| state.log_hydrations.is_empty());
}

/// A child whose turn completes while its long log is still being read
/// reports that turn: the callback to its parent sees the report it made in
/// it.
#[test]
fn a_child_completing_while_its_log_is_read_reports_that_turn() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("child-completing-while-read");
    stored_thread(
        &store,
        "child",
        ProviderKind::Codex,
        &store.root().join("child"),
        |_| {},
    );
    persist_streamed_turns(&store, "child", 1);
    let fleet = Fleet::new(cx, &store, ProviderKind::Codex, &["child"], |_| {});
    fleet.child("child").play([
        report_result_event("rep-new", "the new report"),
        persisted_assistant_event("the new answer"),
        completed_turn("turn-1"),
    ]);
    let callback = fleet.lead.next_steer(cx);
    assert!(callback.starts_with("[orchestrate] thread child"));
    assert!(callback.ends_with("\nthe new report"));
}

/// A message whose attachment cannot be read is not sent: nothing of it
/// reaches the provider or the queue, and the error names the file.
#[test]
fn a_message_with_an_unreadable_attachment_is_not_sent() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("unreadable-attachment");
    stored_thread(
        &store,
        "thread",
        ProviderKind::Codex,
        &store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &store);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    provider.play([completed_turn("turn")]);
    cx.run_until_seen(|view| view.status("thread").running_turn.is_none());
    let readable = store.root().join("readable.png");
    std::fs::write(&readable, b"png").unwrap();
    let missing = store.root().join("missing.png");
    let _ = state.command(
        cx,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "what is in these?".into(),
            attachment_paths: vec![readable, missing.clone()],
        },
    );
    cx.run_until_seen(|view| {
        view.notifications().iter().any(|notification| {
            matches!(notification, RuntimeEvent::Error(RuntimeError::External(error))
                if error.starts_with(&missing.display().to_string()))
        })
    });
    assert!(next_command(&provider.commands).is_err());
    assert!(cx.seen().status("thread").queued_messages.is_empty());
}

/// Each message is recorded with who it came from: the user's, another
/// thread's agent's, and none for what the provider reports on its own; a
/// top-level thread's own provider message counts as the human's.
#[test]
fn message_origins_survive_queue_acknowledgement_and_callback_steering() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("message-origin");
    let fleet = Fleet::new(cx, &store, ProviderKind::Codex, &["child"], |_| {});
    // The child's report is an agent's message to the lead.
    fleet.child("child").play([
        report_result_event("rep-1", "agent completion"),
        completed_turn("turn-1"),
    ]);
    let SessionCommand::Steer { request_id, .. } = fleet.lead.next_command(cx) else {
        panic!("the report is steered into the lead's turn")
    };
    fleet.lead.play([
        AgentEvent::SteerAccepted { request_id },
        // A message the provider reports on its own, such as an answer it
        // steers into the running turn.
        AgentEvent::SteerRequested {
            request_id: "provider-reply".into(),
            text: "A".into(),
            attachments: vec![],
        },
    ]);
    fleet.lead.fence(cx, "parent");
    let messages: Vec<_> = cx
        .seen()
        .records("parent")
        .iter()
        .filter(|record| {
            matches!(
                record.event,
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::UserMessage { .. },
                    ..
                }) | AgentEvent::SteerRequested { .. }
            )
        })
        .cloned()
        .collect();
    assert_eq!(
        messages
            .iter()
            .map(|record| record.origin)
            .collect::<Vec<_>>(),
        [Some(MessageOrigin::Human), Some(MessageOrigin::Agent), None]
    );
    let fold =
        |record: &StoredEvent| tcode_core::settlement::ThreadActivity::fold_stored([record], false);
    assert_eq!(fold(&messages[1]).last_human_message_at, None);
    assert_eq!(
        fold(&messages[2]).last_human_message_at,
        messages[2].ts,
        "a top-level thread's own provider message is the human's"
    );
}

#[test]
fn cold_item_reads_match_folded_streams_and_reused_ids() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("cold-items");
    open_stored_session(&store, "items");
    let fixture = r#"{"type":"turn_started","turn_id":"one"}
{"type":"item_started","id":"c\u006dd","content":{"kind":"command_execution","command":"make","output":"","exit_code":null,"status":"in_progress"}}
{"type":"delta","item_id":"cmd","kind":"command_output","text":"abc"}
{"type":"delta","item_id":"cmd","kind":"command_output","text":"def"}
{"type":"item_updated","id":"cmd","content":{"kind":"command_execution","command":"make","output":"abc","exit_code":null,"status":"in_progress"}}
{"type":"item_completed","id":"cmd","content":{"kind":"command_execution","command":"make","output":"","exit_code":0,"status":"completed"}}
{"type":"item_completed","id":"image","content":{"kind":"image_read","path":"fixture.png","image":{"media_type":"image/png","data_base64":"AQID","source_path":null}}}
{"type":"turn_completed","turn_id":"one","status":"completed","usage":null}
{"type":"turn_started","turn_id":"two"}
{"type":"delta","item_id":"reused","kind":"command_output","text":"old"}
{"type":"turn_completed","turn_id":"two","status":"completed","usage":null}
{"type":"turn_started","turn_id":"three"}
{"type":"turn_checkpoint","turn_id":"three","checkpoint_id":"rewound"}
{"type":"delta","item_id":"reused","kind":"command_output","text":"new"}
{"type":"delta","item_id":"removed","kind":"command_output","text":"gone"}
{"type":"turn_completed","turn_id":"three","status":"completed","usage":null}
"#;
    store
        .apply(&[tcode_services::store::Mutation::replace_event_log(
            "items",
            fixture.as_bytes().to_vec(),
        )])
        .unwrap();
    store.backfill_thread_activity(&["items".into()]).unwrap();
    let whole = SessionLog::new(store.read_log("items").unwrap());
    let read = store.snapshot_log("items").unwrap();
    assert!(read.read_item("reused").unwrap().is_none());
    assert!(read.read_item("removed").unwrap().is_some());
    let targeted = SessionLog::new(
        read.read_item("cmd")
            .unwrap()
            .expect("one physical turn can be read without hydration"),
    );
    assert_eq!(
        targeted.item_output("items", "cmd").unwrap(),
        QueryResponse::ItemOutput("abcdef".into())
    );
    assert_eq!(
        targeted.item_output("items", "cmd"),
        whole.item_output("items", "cmd")
    );
    let targeted = SessionLog::new(read.read_item("image").unwrap().unwrap());
    assert_eq!(
        serde_json::to_value(
            targeted
                .item_image("image", 0, None)
                .unwrap()
                .read()
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(whole.item_image("image", 0, None).unwrap().read().unwrap()).unwrap()
    );
    drop(read);
    let rewind_fixture = format!(
        "{fixture}{}\n",
        r#"{"type":"rewind_completed","checkpoint_id":"rewound","mode":"conversation","prefill":null}"#
    );
    store
        .apply(&[tcode_services::store::Mutation::replace_event_log(
            "rewound",
            rewind_fixture.into_bytes(),
        )])
        .unwrap();
    open_stored_session(&store, "rewound");
    store.backfill_thread_activity(&["rewound".into()]).unwrap();
    let read = store.snapshot_log("rewound").unwrap();
    assert!(read.read_item("removed").unwrap().is_none());
    assert!(
        SessionLog::new(store.read_log("rewound").unwrap())
            .item_output("rewound", "removed")
            .is_err()
    );
    drop(read);
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    for (id, item) in [(1, "reused"), (2, "cmd")] {
        let payload = tcode_protocol::ClientPayload::Query(tcode_protocol::Query::ReadItemOutput {
            session_id: "items".into(),
            item_id: item.into(),
        });
        assert_eq!(
            query_reply(&through_pipe(&state, cx, id, payload), id),
            whole.item_output("items", item).unwrap()
        );
    }
}

#[test]
fn an_unindexed_cold_thread_opens_through_hydration() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("unindexed-history");
    open_stored_session(&store, "legacy");
    store
        .apply(&[tcode_services::store::Mutation::replace_event_log(
            "legacy",
            include_bytes!("../../../../services/src/store/fixtures/turn-index.jsonl").to_vec(),
        )])
        .unwrap();
    assert!(store.turn_index("legacy").unwrap().is_none());
    let whole = SessionLog::new(store.read_log("legacy").unwrap());
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let window = window_reply(
        &through_pipe(&state, cx, 1, subscribe_events("legacy", None)),
        1,
    );
    assert_eq!(
        window,
        window_of(&whole.events_window(
            &Subscription {
                topic: Topic::SessionEvents {
                    session_id: "legacy".into()
                },
                after: None,
                known_index: None,
            },
            None,
        ))
        .unwrap()
    );
    let page = query_reply(
        &through_pipe(&state, cx, 2, history_page("legacy", window.end)),
        2,
    );
    assert_eq!(page, whole.history_page(window.end, 200, None));
    assert!(store.turn_index("legacy").unwrap().is_none());
}
