use super::test_support::*;
use super::*;
use super::{active_session::*, events::*, orchestrate::*, providers::*};

use tcode_core::notice::CHILD_REPORT_REMINDER;
use tcode_core::project::ChildExpectation;
use tcode_core::settings::{SettingsPatch, ThemeMode};
use tcode_protocol::{BackgroundTaskStatus, Command, CommandResponse, HostMessage, Subscription};
#[test]
fn permission_relaunch_marker_requires_screen_access_only_for_computer_use() {
    for screen_recording in [false, true] {
        let permissions = computer_use_mcp::permissions::PermissionStatus {
            screen_recording,
            ..Default::default()
        };
        assert_eq!(permission_relaunch_marker(None, permissions), None);
        for section in ["computer_use", "providers"] {
            let marker = tcode_services::relaunch::RelaunchMarker {
                reopen_settings: section.into(),
                active_session: Some("session-1".into()),
            };
            let expected =
                (section != "computer_use" || screen_recording).then_some(marker.clone());
            assert_eq!(
                permission_relaunch_marker(Some(marker), permissions),
                expected
            );
        }
    }
}

/// What crosses the wire for `thread`'s index entry while `events` run, one
/// host turn each, as the provider delivers them to `via`.
fn published_activity(
    state: &TestEntity,
    cx: &mut TestAppContext,
    via: &str,
    thread: &str,
    events: Vec<AgentEvent>,
) -> Vec<tcode_protocol::SessionActivity> {
    let mut published = Vec::new();
    for event in events {
        state.update(cx, |state, cx| {
            state.provider_event_for_test(via, event, cx);
        });
        cx.run_until_parked();
        for message in cx.drain_outgoing() {
            if let HostMessage::Event(envelope) = message
                && let ServerEvent::IndexActivitiesChanged { upserts, .. } = envelope.event
                && let Some(activity) = upserts.get(thread)
            {
                published.push(activity.clone());
            }
        }
    }
    published
}

fn subscribed_to_index(cx: &mut TestAppContext, state: TestClientState) -> TestEntity {
    let state = cx.new_entity(state);
    state.deliver(
        cx,
        1,
        tcode_protocol::ClientPayload::Subscribe(Subscription {
            topic: Topic::Index,
            after: None,
            known_index: None,
        }),
    );
    cx.run_until_parked();
    cx.drain_outgoing();
    state
}

/// A client learns what a thread it does not hold is doing from the index
/// alone, and only when that changes: a stream sends nothing, the next item
/// sends one update, an item saying the same again sends none, and the
/// turn's end clears it.
#[test]
fn only_a_new_item_publishes_a_threads_latest_activity() {
    use tcode_core::session::LatestActivity;
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-latest-activity");
    let mut host = TestClientState::new((*test_store).clone());
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/child"), None);
    meta.id = "child".into();
    host.sessions.push(meta.clone());
    host.install_selected(ActiveSession::new(meta, false, Vec::new()));
    let state = subscribed_to_index(cx, host);

    let item = |id: &str, content| {
        AgentEvent::ItemStarted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        })
    };
    let command = |id: &str| {
        item(
            id,
            ItemContent::CommandExecution {
                command: "cargo test".into(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::InProgress,
            },
        )
    };
    let delta = |item_id: &str, kind, text: &str| AgentEvent::Delta {
        item_id: item_id.into(),
        kind,
        text: text.into(),
    };
    published_activity(
        &state,
        cx,
        "child",
        "child",
        vec![AgentEvent::TurnStarted {
            turn_id: "turn".into(),
        }],
    );

    let activities = |published: Vec<tcode_protocol::SessionActivity>| {
        published
            .into_iter()
            .map(|activity| activity.latest_activity)
            .collect::<Vec<_>>()
    };
    let thought = item(
        "thought",
        ItemContent::Reasoning {
            text: String::new(),
        },
    );
    assert_eq!(
        activities(published_activity(
            &state,
            cx,
            "child",
            "child",
            vec![thought]
        )),
        [Some(LatestActivity::Thinking)]
    );
    let stream = (0..50)
        .map(|_| delta("thought", agent::DeltaKind::ReasoningText, "more"))
        .collect();
    assert_eq!(
        activities(published_activity(&state, cx, "child", "child", stream)),
        []
    );
    let running = LatestActivity::Command {
        text: "cargo test".into(),
    };
    assert_eq!(
        activities(published_activity(
            &state,
            cx,
            "child",
            "child",
            vec![
                command("first"),
                delta("first", agent::DeltaKind::CommandOutput, "ok\n"),
                command("second"),
            ]
        )),
        [Some(running)]
    );
    assert_eq!(
        activities(published_activity(
            &state,
            cx,
            "child",
            "child",
            vec![AgentEvent::TurnCompleted {
                turn_id: "turn".into(),
                status: TurnStatus::Completed,
                usage: None,
            }]
        ))
        .last(),
        Some(&None)
    );
}

/// A native subagent's work lands in its mirror, so that is the thread whose
/// activity tells it; the lead's own entry stays with the spawn it started.
/// When the subagent ends so does what it was doing.
#[test]
fn a_native_subagents_activity_is_published_on_its_mirror() {
    use tcode_core::session::LatestActivity;
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-mirror-activity");
    let mut host = TestClientState::new((*test_store).clone());
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/tmp/lead"), None);
    meta.id = "lead".into();
    host.sessions.push(meta.clone());
    host.install_selected(ActiveSession::new(meta, false, Vec::new()));
    let state = subscribed_to_index(cx, host);

    let spawn = |status| ThreadItem {
        id: "spawn".into(),
        parent_item_id: None,
        content: ItemContent::Subagent {
            agent_type: "explorer".into(),
            description: "Audit".into(),
            status,
            summary: None,
            model: None,
            effort: None,
        },
    };
    published_activity(
        &state,
        cx,
        "lead",
        "lead",
        vec![
            AgentEvent::TurnStarted {
                turn_id: "turn".into(),
            },
            AgentEvent::ItemStarted(spawn(ItemStatus::InProgress)),
        ],
    );
    let mirror = state.read(|state| {
        state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.is_some())
            .expect("the spawn made a mirror")
            .id
            .clone()
    });
    let published = published_activity(
        &state,
        cx,
        "lead",
        &mirror,
        vec![AgentEvent::ItemStarted(ThreadItem {
            id: "child".into(),
            parent_item_id: Some("spawn".into()),
            content: ItemContent::CommandExecution {
                command: "ls".into(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::InProgress,
            },
        })],
    );
    assert_eq!(
        published.last().unwrap().latest_activity,
        Some(LatestActivity::Command { text: "ls".into() })
    );
    state.read(|state| {
        assert_eq!(
            state.resident("lead").unwrap().timeline.latest_activity(),
            Some(LatestActivity::Subagent {
                description: "Audit".into()
            })
        );
    });

    let published = published_activity(
        &state,
        cx,
        "lead",
        &mirror,
        vec![AgentEvent::ItemCompleted(spawn(ItemStatus::Completed))],
    );
    assert_eq!(published.last().unwrap().latest_activity, None);
}

/// A log whose turn a dead host left open still folds to a running timeline,
/// but nothing is running it, so the thread reports no activity.
#[test]
fn a_turn_left_open_in_the_log_reports_no_activity() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-stale-activity");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    state.update(cx, |state, _| {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/stale"), None);
        meta.id = "stale".into();
        let mut session = ActiveSession::new(meta.clone(), false, Vec::new());
        for event in [
            AgentEvent::TurnStarted {
                turn_id: "turn".into(),
            },
            AgentEvent::ItemStarted(ThreadItem {
                id: "thought".into(),
                parent_item_id: None,
                content: ItemContent::Reasoning {
                    text: String::new(),
                },
            }),
        ] {
            session.timeline.apply_at(None, &event);
        }
        state.sessions.push(meta);
        state.install_selected(session);
    });
    let summary = state.read(|state| state.index_snapshot().summary);
    assert_eq!(summary.activity["stale"].latest_activity, None);
}

#[test]
fn provider_native_subagent_events_create_and_feed_read_only_mirror_session() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-native-subagent-mirror-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    state.update(cx, |state, cx| {
        let mut parent_meta = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/native-subagent-parent"),
            Some("gpt-test".into()),
        );
        parent_meta.id = "parent".into();
        parent_meta.project_id = Some("project".into());
        parent_meta.option_selections = vec![OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("high"),
        }];
        state.sessions.push(parent_meta.clone());
        state.install_selected(ActiveSession::new(parent_meta, false, Vec::new()));

        state.on_event(
            "parent",
            AgentEvent::ItemStarted(ThreadItem {
                id: "spawn-1".into(),
                parent_item_id: None,
                content: ItemContent::Subagent {
                    agent_type: "explorer".into(),
                    description: "Inspect event routing\nand report".into(),
                    status: ItemStatus::InProgress,
                    summary: None,
                    model: None,
                    effort: None,
                },
            }),
            cx,
        );

        let mirror = state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.as_deref() == Some("spawn-1"))
            .cloned()
            .expect("native subagent mirror metadata");
        assert_eq!(mirror.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(mirror.title, "explorer: Inspect event routing");
        assert_eq!(mirror.option_selections[0].value, serde_json::json!("high"));
        assert_eq!(mirror.model.as_deref(), Some("gpt-test"));

        state.on_event(
            "parent",
            AgentEvent::ItemUpdated(ThreadItem {
                id: "spawn-1".into(),
                parent_item_id: None,
                content: ItemContent::Subagent {
                    agent_type: "explorer".into(),
                    description: "Inspect event routing".into(),
                    status: ItemStatus::InProgress,
                    summary: None,
                    model: Some("child-model".into()),
                    effort: Some("low".into()),
                },
            }),
            cx,
        );
        let status = state.session_status_snapshot(&mirror.id).unwrap();
        assert_eq!(status.requested_model.as_deref(), Some("child-model"));
        assert_eq!(
            status.provider_option_selections[0].value,
            serde_json::json!("low")
        );
        assert_eq!(
            state.resident("parent").unwrap().meta.model.as_deref(),
            Some("gpt-test")
        );
        assert!(state.resident(&mirror.id).unwrap().has_work());
        assert!(state.resident(&mirror.id).unwrap().timeline.turn_running);

        state.on_event(
            "parent",
            AgentEvent::ItemCompleted(ThreadItem {
                id: "child-answer".into(),
                parent_item_id: Some("spawn-1".into()),
                content: ItemContent::AssistantMessage {
                    text: "routed transcript".into(),
                    summarized: false,
                },
            }),
            cx,
        );
        let mirror_timeline = &state.resident(&mirror.id).unwrap().timeline;
        assert!(mirror_timeline.entries.iter().any(|entry| {
            matches!(
                &entry.content,
                EntryContent::Item(ItemContent::AssistantMessage { text, .. })
                    if entry.id == "child-answer" && text == "routed transcript"
            )
        }));
        assert_eq!(state.resident("parent").unwrap().timeline.entries.len(), 1);

        state.on_event(
            "parent",
            AgentEvent::ItemCompleted(ThreadItem {
                id: "spawn-1".into(),
                parent_item_id: None,
                content: ItemContent::Subagent {
                    agent_type: "explorer".into(),
                    description: "Inspect event routing\nand report".into(),
                    status: ItemStatus::Completed,
                    summary: Some("routing verified".into()),
                    model: None,
                    effort: None,
                },
            }),
            cx,
        );
        assert!(!state.resident(&mirror.id).unwrap().has_work());
        assert!(!state.resident(&mirror.id).unwrap().timeline.turn_running);
        assert!(matches!(
            &state.resident("parent").unwrap().timeline.entries[0].content,
            EntryContent::Item(ItemContent::Subagent {
                status: ItemStatus::Completed,
                summary: Some(summary),
                ..
            }) if summary == "routing verified"
        ));
    });
    cx.run_until_parked();

    state.update(cx, |state, _| {
        let mirror = state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.as_deref() == Some("spawn-1"))
            .unwrap();
        // A completion without model/effort must not erase the last observation,
        // and reopening the persisted mirror must retain it.
        let persisted = state
            .store
            .load_index()
            .unwrap()
            .into_iter()
            .find(|meta| meta.id == mirror.id)
            .unwrap();
        assert_eq!(persisted.model.as_deref(), Some("child-model"));
        assert_eq!(
            persisted.option_selections[0].value,
            serde_json::json!("low")
        );
        let mirror_events = state.store.read_events(&mirror.id).unwrap();
        assert!(mirror_events.iter().any(|stored| matches!(
            &stored.event,
            AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                ..
            }) if id == "child-answer"
        )));
        let parent_events = state.store.read_events("parent").unwrap();
        assert!(parent_events.iter().any(|stored| matches!(
            &stored.event,
            AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::Subagent { .. },
            }) if id == "spawn-1"
        )));
        assert!(parent_events.iter().all(|stored| match &stored.event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => item.parent_item_id.is_none(),
            _ => true,
        }));
    });
}

/// The canonical events OpenCode's recorded `subagent_turn.jsonl` maps to
/// (asserted by `opencode::tests::a_subagents_activity_nests_under_the_call_that_spawned_it`):
/// the child's text arrives as `ItemStarted` / `Delta` / `ItemCompleted`, and
/// only the two snapshots carry `parent_item_id`. The child's reply belongs to
/// its mirror; the parent shows only the spawn and its own reply.
#[test]
fn a_subagents_streamed_text_stays_out_of_the_parent_thread() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-opencode-subagent-deltas");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let spawn = "msg_198acc1db0013V4MEKRcfmYvtt:chatcmpl-tool-b5f7774ecdb70d58";
    let child = "msg_198accaf2001JTZdDPBbxetEk2:text:0";
    let reply = "msg_198accd89001hG3ffj4MheRnb9:text:0";
    let subagent = |status, summary: Option<&str>| ThreadItem {
        id: spawn.into(),
        parent_item_id: None,
        content: ItemContent::Subagent {
            agent_type: "general".into(),
            description: "Reply with KIWI".into(),
            status,
            summary: summary.map(Into::into),
            model: Some("opencode/deepseek-v4.1-flash-free".into()),
            effort: None,
        },
    };
    let text = |id: &str, parent: Option<&str>, text: &str| ThreadItem {
        id: id.into(),
        parent_item_id: parent.map(Into::into),
        content: ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        },
    };
    let delta = |id: &str, text: &str| AgentEvent::Delta {
        item_id: id.into(),
        kind: agent::DeltaKind::AssistantText,
        text: text.into(),
    };
    state.update(cx, |state, cx| {
        let mut meta = SessionMeta::new(ProviderKind::OpenCode, PathBuf::from("/tmp"), None);
        meta.id = "parent".into();
        state.sessions.push(meta.clone());
        state.install_selected(ActiveSession::new(meta, false, Vec::new()));
        for event in [
            AgentEvent::TurnStarted {
                turn_id: "opencode-turn-1".into(),
            },
            AgentEvent::ItemStarted(subagent(ItemStatus::InProgress, None)),
            AgentEvent::ItemStarted(text(child, Some(spawn), "")),
            delta(child, "KIWI"),
            AgentEvent::ItemCompleted(text(child, Some(spawn), "KIWI")),
            AgentEvent::ItemCompleted(subagent(ItemStatus::Completed, Some("KIWI"))),
            AgentEvent::ItemStarted(text(reply, None, "")),
            delta(reply, "The subagent said: KIWI"),
            AgentEvent::ItemCompleted(text(reply, None, "The subagent said: KIWI")),
            AgentEvent::TurnCompleted {
                turn_id: "opencode-turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ] {
            state.on_event("parent", event, cx);
        }
    });
    cx.run_until_parked();
    let ids = |timeline: &Timeline| -> Vec<String> {
        timeline
            .entries
            .iter()
            .filter(|entry| matches!(entry.content, EntryContent::Item(_)))
            .map(|entry| entry.id.clone())
            .collect()
    };
    state.update(cx, |state, _| {
        let mirror = state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.as_deref() == Some(spawn))
            .unwrap()
            .id
            .clone();
        assert_eq!(
            ids(&Timeline::fold_events(
                state.store.read_events(&mirror).unwrap()
            )),
            [child],
            "the mirror holds the child's reply"
        );
        assert_eq!(
            ids(&state.resident("parent").unwrap().timeline),
            [spawn, reply],
            "the live parent thread"
        );
        assert_eq!(
            ids(&Timeline::fold_events(
                state.store.read_events("parent").unwrap()
            )),
            [spawn, reply],
            "the parent thread reopened from its log"
        );
    });
}

/// How the mirror's single turn ends: the Subagent item reaching a terminal
/// status, or the parent process closing while it is still running. A parent
/// `TurnCompleted` is never an ending — a background subagent outlives it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MirrorEnd {
    SubagentCompleted,
    /// Parent turn completes first (background subagent), then a child item
    /// arrives, then the Subagent item completes.
    ParentTurnThenSubagentCompleted,
    SessionClosed,
}

// Exercise provider routing and disk replay, including residency changes between events.
fn assert_native_mirror_turn_lifecycle(evict: bool, late: bool, end: MirrorEnd, reload: bool) {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-native-mirror-turn-lifecycle");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let mirror_id = state.update(cx, |state, cx| {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp"), None);
        meta.id = "parent".into();
        state.sessions.push(meta.clone());
        state.install_selected(ActiveSession::new(meta, false, Vec::new()));
        state.on_event(
            "parent",
            AgentEvent::TurnStarted {
                turn_id: "parent-turn".into(),
            },
            cx,
        );
        state.on_event(
            "parent",
            native_mirror_parent_item(ItemStatus::InProgress),
            cx,
        );
        let id = state
            .sessions
            .iter()
            .find(|m| m.native_subagent.is_some())
            .unwrap()
            .id
            .clone();
        for i in 0..3 {
            if evict && i == 1 {
                assert!(state.residents.evict(&id).is_some());
                assert!(state.resident(&id).is_none());
                if reload {
                    // Force metadata lookup and its asynchronous timeline load.
                    state.native_subagent_sessions.clear();
                }
            }
            state.on_event("parent", native_mirror_child_item(i), cx);
        }
        match end {
            MirrorEnd::SubagentCompleted => {
                state.on_event(
                    "parent",
                    native_mirror_parent_item(ItemStatus::Completed),
                    cx,
                );
            }
            MirrorEnd::ParentTurnThenSubagentCompleted => {
                state.on_event(
                    "parent",
                    AgentEvent::TurnCompleted {
                        turn_id: "parent-turn".into(),
                        status: TurnStatus::Completed,
                        usage: None,
                    },
                    cx,
                );
                assert_eq!(
                    state.native_subagent_turns.get(&id),
                    Some(&true),
                    "background subagent keeps running after the parent turn"
                );
                assert!(evict || state.turn_running_for(&id));
                state.on_event("parent", native_mirror_child_item(3), cx);
                state.on_event(
                    "parent",
                    native_mirror_parent_item(ItemStatus::Completed),
                    cx,
                );
            }
            MirrorEnd::SessionClosed => {
                state.on_event("parent", AgentEvent::SessionClosed { reason: None }, cx);
            }
        }
        if late {
            state.on_event("parent", native_mirror_child_item(4), cx);
        }
        id
    });
    cx.run_until_parked();
    let expected_items =
        3 + usize::from(end == MirrorEnd::ParentTurnThenSubagentCompleted) + usize::from(late);
    let expected_status = if end == MirrorEnd::SessionClosed {
        TurnStatus::Interrupted
    } else {
        TurnStatus::Completed
    };
    state.update(cx, |state, cx| {
        let events = state.store.read_events(&mirror_id).unwrap();
        let mut open = false;
        let mut boundaries = 0;
        let mut items = 0;
        for stored in &events {
            match &stored.event {
                AgentEvent::TurnStarted { .. } => {
                    assert!(!open);
                    open = true;
                    boundaries += 1;
                }
                AgentEvent::TurnCompleted { status, .. } => {
                    assert!(open);
                    assert_eq!(*status, expected_status);
                    open = false;
                    boundaries += 1;
                }
                AgentEvent::ItemCompleted(_) => {
                    items += 1;
                }
                _ => {}
            }
        }
        assert!(!open, "persisted mirror boundaries must balance");
        assert_eq!(boundaries, 2, "exactly one turn, never a zero-length one");
        assert_eq!(items, expected_items);
        assert!(!state.turn_running_for(&mirror_id));
        if state.resident(&mirror_id).is_none() {
            let meta = state.find_meta(&mirror_id).unwrap().clone();
            state.load_background_session(meta, cx);
        }
    });
    // The turn's end can show before its last reply is folded in.
    let replies = |mirror: &ActiveSession| {
        mirror
            .timeline
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.content,
                    EntryContent::Item(ItemContent::AssistantMessage { .. })
                )
            })
            .count()
    };
    cx.run_until(|state| {
        state.resident(&mirror_id).is_some_and(|mirror| {
            replies(mirror) == expected_items
                && mirror
                    .timeline
                    .turns
                    .first()
                    .is_some_and(|turn| turn.end_ts.is_some())
        })
    });
    state.update(cx, |state, _| {
        let mirror = state.resident(&mirror_id).unwrap();
        assert!(!mirror.timeline.turn_running);
        assert_eq!(mirror.timeline.turns.len(), 1);
        let turn = &mirror.timeline.turns[0];
        assert!(!turn.running && turn.start_ts.is_some() && turn.end_ts.is_some());
        assert_eq!(turn.status, Some(expected_status));
        assert!(!mirror.turn_in_flight);
        assert!(!mirror.has_work());
        assert!(
            !state
                .session_status_snapshot(&mirror_id)
                .unwrap()
                .activity
                .working
        );
    });
}

fn native_mirror_parent_item(status: ItemStatus) -> AgentEvent {
    let item = ThreadItem {
        id: "spawn-1".into(),
        parent_item_id: None,
        content: ItemContent::Subagent {
            agent_type: "explorer".into(),
            description: "Inspect routing".into(),
            status,
            summary: None,
            model: None,
            effort: None,
        },
    };
    if status == ItemStatus::InProgress {
        AgentEvent::ItemStarted(item)
    } else {
        AgentEvent::ItemCompleted(item)
    }
}

fn native_mirror_child_item(i: usize) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: format!("child-{i}"),
        parent_item_id: Some("spawn-1".into()),
        content: ItemContent::AssistantMessage {
            text: format!("answer {i}"),
            summarized: false,
        },
    })
}

#[test]
fn native_mirror_keeps_one_turn_across_residency_late_items_and_parent_completion() {
    for (evict, reload) in [(false, false), (true, false), (true, true)] {
        for late in [false, true] {
            for end in [
                MirrorEnd::SubagentCompleted,
                MirrorEnd::ParentTurnThenSubagentCompleted,
                MirrorEnd::SessionClosed,
            ] {
                eprintln!(
                    "mirror lifecycle: evict={evict}, reload={reload}, late={late}, end={end:?}"
                );
                assert_native_mirror_turn_lifecycle(evict, late, end, reload);
            }
        }
    }
}

fn subagent_item(
    id: &str,
    parent_item_id: Option<&str>,
    agent_type: &str,
    description: &str,
    status: ItemStatus,
) -> ThreadItem {
    ThreadItem {
        id: id.into(),
        parent_item_id: parent_item_id.map(str::to_owned),
        content: ItemContent::Subagent {
            agent_type: agent_type.into(),
            description: description.into(),
            status,
            summary: None,
            model: None,
            effort: None,
        },
    }
}

/// A subagent's transcript can itself spawn a subagent. The nested spawn is
/// content of the child mirror and, from the spawn on, has a titled mirror
/// under the child mirror — never a nameless one on the root — so the child
/// thread's row for it opens while it runs, before (or without) any of the
/// grandchild's own transcript. Its turn closes with the nested spawn's
/// terminal status.
#[test]
fn nested_subagent_items_open_a_titled_mirror_under_the_child_mirror_and_close_it() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-nested-native-mirror");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (child_id, grandchild_id, second_id) = state.update(cx, |state, cx| {
        let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/tmp"), None);
        meta.id = "parent".into();
        state.sessions.push(meta.clone());
        state.install_selected(ActiveSession::new(meta, false, Vec::new()));
        state.on_event(
            "parent",
            AgentEvent::ItemStarted(subagent_item(
                "toolu_child",
                None,
                "general-purpose",
                "Research hosting",
                ItemStatus::InProgress,
            )),
            cx,
        );
        state.on_event(
            "parent",
            AgentEvent::ItemStarted(subagent_item(
                "toolu_grandchild",
                Some("toolu_child"),
                "Explore",
                "Research Workers limits\nwith sources",
                ItemStatus::InProgress,
            )),
            cx,
        );
        let child_id = state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.as_deref() == Some("toolu_child"))
            .unwrap()
            .id
            .clone();
        let grandchild = state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.as_deref() == Some("toolu_grandchild"))
            .cloned()
            .expect("the spawn alone opens the grandchild's mirror");
        assert_eq!(
            grandchild.parent_session_id.as_deref(),
            Some(child_id.as_str())
        );
        assert_eq!(grandchild.title, "Explore: Research Workers limits");
        assert!(state.resident(&grandchild.id).unwrap().has_work());
        assert!(
            state
                .resident(&child_id)
                .unwrap()
                .timeline
                .entries
                .iter()
                .any(|entry| {
                    entry.id == "toolu_grandchild"
                        && matches!(
                            entry.content,
                            EntryContent::Item(ItemContent::Subagent { .. })
                        )
                })
        );
        // Launched in the background, it is the child's background work.
        state.on_event(
            "parent",
            AgentEvent::ItemBackgrounded {
                item_id: "toolu_grandchild".into(),
                parent_item_id: Some("toolu_child".into()),
            },
            cx,
        );
        assert!(matches!(
            state.session_status_snapshot(&child_id).unwrap().background_tasks.as_slice(),
            [listed] if listed.task.item_id.as_deref() == Some("toolu_grandchild")
                && listed.task.kind == agent::BackgroundKind::Agent
        ));
        assert!(
            state
                .session_status_snapshot("parent")
                .unwrap()
                .background_tasks
                .is_empty()
        );

        state.on_event(
            "parent",
            AgentEvent::ItemCompleted(ThreadItem {
                id: "toolu_grandchild:user-1".into(),
                parent_item_id: Some("toolu_grandchild".into()),
                content: ItemContent::UserMessage {
                    text: "Fetch the limits page.".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            cx,
        );
        assert!(
            state
                .resident(&grandchild.id)
                .unwrap()
                .timeline
                .entries
                .iter()
                .any(|entry| entry.id == "toolu_grandchild:user-1"),
            "the grandchild's transcript goes to the mirror its spawn opened"
        );
        assert_eq!(
            state
                .sessions
                .iter()
                .filter(|meta| meta.native_subagent.as_deref() == Some("toolu_grandchild"))
                .count(),
            1
        );
        assert!(state.resident(&grandchild.id).unwrap().has_work());
        assert!(state.resident(&child_id).unwrap().has_work());
        assert_eq!(
            state
                .sessions
                .iter()
                .filter(|meta| meta.parent_session_id.as_deref() == Some("parent"))
                .count(),
            1,
            "the root session owns only the child it spawned"
        );

        // The grandchild's terminal snapshot travels as child content too.
        state.on_event(
            "parent",
            AgentEvent::ItemUpdated(subagent_item(
                "toolu_grandchild",
                Some("toolu_child"),
                "Explore",
                "Research Workers limits",
                ItemStatus::Completed,
            )),
            cx,
        );
        assert!(!state.resident(&grandchild.id).unwrap().has_work());
        assert!(state.resident(&child_id).unwrap().has_work());
        state.on_event(
            "parent",
            AgentEvent::ItemCompleted(subagent_item(
                "toolu_child",
                None,
                "general-purpose",
                "Research hosting",
                ItemStatus::Completed,
            )),
            cx,
        );
        assert!(!state.resident(&child_id).unwrap().has_work());
        assert_eq!(state.resident("parent").unwrap().timeline.entries.len(), 1);

        // A second nested spawn is still running when the root process closes:
        // it belongs to that process, so it ends with it.
        for event in [
            AgentEvent::ItemStarted(subagent_item(
                "toolu_grandchild_2",
                Some("toolu_child"),
                "Explore",
                "Check pricing",
                ItemStatus::InProgress,
            )),
            AgentEvent::ItemCompleted(ThreadItem {
                id: "toolu_grandchild_2:msg:0".into(),
                parent_item_id: Some("toolu_grandchild_2".into()),
                content: ItemContent::AssistantMessage {
                    text: "Fetching pricing.".into(),
                    summarized: false,
                },
            }),
        ] {
            state.on_event("parent", event, cx);
        }
        let second = state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.as_deref() == Some("toolu_grandchild_2"))
            .cloned()
            .expect("second grandchild mirror");
        assert_eq!(second.parent_session_id.as_deref(), Some(child_id.as_str()));
        assert!(state.resident(&second.id).unwrap().has_work());
        state.on_event("parent", AgentEvent::SessionClosed { reason: None }, cx);
        assert!(
            !state
                .resident(&second.id)
                .is_some_and(ActiveSession::has_work)
        );
        (child_id, grandchild.id, second.id)
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let events = state.store.read_events(&grandchild_id).unwrap();
        assert!(matches!(
            &events.first().unwrap().event,
            AgentEvent::TurnStarted { turn_id } if turn_id == "toolu_grandchild"
        ));
        assert!(matches!(
            &events.last().unwrap().event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        ));
        assert!(matches!(
            &state.store.read_events(&second_id).unwrap().last().unwrap().event,
            AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Interrupted, .. }
                if turn_id == "toolu_grandchild_2"
        ));
        assert!(state.store.read_events(&child_id).unwrap().iter().all(
            |stored| match &stored.event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item) => item.parent_item_id.is_none(),
                _ => true,
            }
        ));
    });
}

/// A child item whose spawn this session never announced still gets a mirror,
/// and the parent process closing ends it like every other running mirror.
#[test]
fn unknown_parent_mirror_closes_when_the_parent_session_closes() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-unknown-parent-native-mirror");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let mirror_id = state.update(cx, |state, cx| {
        let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/tmp"), None);
        meta.id = "parent".into();
        state.sessions.push(meta.clone());
        state.install_selected(ActiveSession::new(meta, false, Vec::new()));
        state.on_event(
            "parent",
            AgentEvent::ItemCompleted(ThreadItem {
                id: "toolu_orphan:msg:0".into(),
                parent_item_id: Some("toolu_orphan".into()),
                content: ItemContent::AssistantMessage {
                    text: "working".into(),
                    summarized: false,
                },
            }),
            cx,
        );
        let mirror = state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.as_deref() == Some("toolu_orphan"))
            .cloned()
            .expect("placeholder mirror");
        assert_eq!(mirror.title, "subagent");
        assert!(state.resident(&mirror.id).unwrap().has_work());
        state.on_event("parent", AgentEvent::SessionClosed { reason: None }, cx);
        assert!(
            !state
                .resident(&mirror.id)
                .is_some_and(ActiveSession::has_work)
        );
        mirror.id
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let events = state.store.read_events(&mirror_id).unwrap();
        assert!(matches!(
            &events.last().unwrap().event,
            AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Interrupted, .. }
                if turn_id == "toolu_orphan"
        ));
    });
}

/// A mirror persisted with an open turn by a host that stopped before the
/// subagent settled: loading it after a restart ends the turn, because no
/// live parent is tracking it and nothing else ever will.
#[test]
fn loading_a_mirror_with_an_open_turn_and_no_live_parent_ends_it() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orphaned-native-mirror-repair");
    let mut parent = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/tmp"), None);
    parent.id = "parent".into();
    let mut mirror = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/tmp"), None);
    mirror.id = "mirror".into();
    mirror.title = "subagent".into();
    mirror.parent_session_id = Some("parent".into());
    mirror.native_subagent = Some("toolu_zombie".into());
    test_store.upsert_meta(&parent).unwrap();
    test_store.upsert_meta(&mirror).unwrap();
    let stored = [
        AgentEvent::TurnStarted {
            turn_id: "toolu_zombie".into(),
        },
        AgentEvent::ItemCompleted(ThreadItem {
            id: "toolu_zombie:msg:0".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "Research complete.".into(),
                summarized: false,
            },
        }),
    ];
    for (i, event) in stored.iter().enumerate() {
        test_store
            .append_event("mirror", 1_000 + i as u64, event)
            .unwrap();
    }

    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    state.update(cx, |state, cx| {
        assert_eq!(state.sessions.len(), 2);
        state.select_session("mirror", cx);
    });
    cx.run_until(|state| {
        state
            .resident("mirror")
            .is_some_and(|mirror| mirror.timeline.turns.len() == 1)
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let mirror = state.resident("mirror").unwrap();
        assert!(!mirror.has_work());
        assert!(!mirror.timeline.turn_running);
        assert_eq!(
            mirror.timeline.turns[0].status,
            Some(TurnStatus::Interrupted)
        );
        assert_eq!(
            mirror.timeline.last_turn_status,
            Some(TurnStatus::Interrupted)
        );
        let events = state.store.read_events("mirror").unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[2].event,
            AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Interrupted, .. }
                if turn_id == "toolu_zombie"
        ));
        assert_eq!(state.native_subagent_turns.get("mirror"), Some(&false));
    });

    // Reloading the repaired mirror leaves the closed turn alone, in a host
    // started again, which nothing the first one left running reaches.
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    state.update(cx, |state, cx| state.select_session("mirror", cx));
    cx.run_until(|state| {
        state
            .resident("mirror")
            .is_some_and(|mirror| mirror.timeline.turns.len() == 1)
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        assert_eq!(state.store.read_events("mirror").unwrap().len(), 3);
    });
}

#[test]
fn settings_patches_preserve_top_level_and_nested_siblings_over_the_pipe() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-settings-patch-isolation");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        state.update_settings(
            Settings {
                sidebar_collapsed: true,
                ..Settings::default()
            },
            cx,
        )
    });
    cx.run_until_parked();
    cx.drain_outgoing();
    for (id, patch) in [
        (41, SettingsPatch::ThemeMode(ThemeMode::Dark)),
        (
            42,
            SettingsPatch::BrowserHomeUrl(Some("https://example.com".into())),
        ),
        (43, SettingsPatch::BrowserAllowEvaluate(false)),
    ] {
        state.dispatch_command(cx, id, Command::PatchSettings { patch });
    }
    cx.run_until_parked();
    let outgoing = cx.drain_outgoing();
    let accepted: Vec<_> = outgoing
        .iter()
        .filter_map(|message| match message {
            HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(accepted, [41, 42, 43]);
    assert!(outgoing.iter().any(|message| matches!(message,
        HostMessage::Event(EventEnvelope {
            request_id: None, topic: Topic::Settings,
            event: ServerEvent::SettingsReplaced(settings), ..
        }) if settings.sidebar_collapsed && settings.theme_mode == ThemeMode::Dark
            && settings.browser.home_url.as_deref() == Some("https://example.com")
            && !settings.browser.allow_evaluate
    )));
}

#[test]
fn reset_settings_clears_preferences_but_keeps_credentials_installs_and_unknown_keys() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-reset-settings-scope-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    // Preferences the reset must clear.
    let mut settings = Settings {
        theme_mode: ThemeMode::Dark,
        language: Some("zh-CN".into()),
        word_wrap_diffs: true,
        sidebar_provider_marks: true,
        ..Settings::default()
    };
    settings.browser.home_url = Some("https://example.com".into());
    // Everything below cost the user a login, a download, or a newer build.
    settings
        .provider_mut(ProviderKind::Codex)
        .env
        .push(tcode_core::settings::EnvVar {
            name: "OPENAI_API_KEY".into(),
            value: "secret".into(),
            sensitive: true,
        });
    settings.profiles.insert(
        "work-claude".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings::default(),
        },
    );
    settings.codex_binary = Some(PathBuf::from("/custom/codex"));
    settings.claude_binary = Some(PathBuf::from("/custom/claude"));
    settings.acp_agents.insert(
        "first".into(),
        InstalledAgent {
            id: "first".into(),
            name: "First".into(),
            version: "1.2.3".into(),
            icon: None,
            launch: agent::AcpLaunch::Npx {
                package: "first-agent".into(),
                args: Vec::new(),
                env: Vec::new(),
            },
            enabled: true,
            env: Vec::new(),
            launch_args: None,
        },
    );
    settings.collapsed_projects.push("project".into());
    let favorite = FavoriteModel::Row {
        provider: ProviderKind::Codex,
        profile_id: Some("work-codex".into()),
        model: "gpt-5.6-sol".into(),
    };
    settings.favorite_models.push(favorite.clone());
    settings.sidebar_collapsed = true;
    settings.project_sort = tcode_core::settings::ProjectSort::NameAsc;
    settings.sidebar_layout = tcode_core::settings::SidebarLayout::Grouped;
    settings.last_visited.insert("session".into(), 42);
    settings
        .unknown
        .insert("future_key".into(), serde_json::json!({"kept": true}));

    state.update(cx, |state, cx| {
        state.update_settings(settings, cx);
        state.reset_settings(cx);

        let reset = &state.settings;
        assert_eq!(reset.theme_mode, ThemeMode::System);
        assert_eq!(reset.language, None);
        assert!(!reset.word_wrap_diffs);
        assert!(!reset.sidebar_provider_marks);
        assert_eq!(reset.browser.home_url, None);

        assert_eq!(
            reset.provider(ProviderKind::Codex).env[0].value,
            "secret",
            "provider credentials must survive a restore"
        );
        assert!(reset.profiles.contains_key("work-claude"));
        assert_eq!(reset.codex_binary, Some(PathBuf::from("/custom/codex")));
        assert_eq!(reset.claude_binary, Some(PathBuf::from("/custom/claude")));
        assert!(reset.acp_agents.contains_key("first"));
        assert_eq!(reset.collapsed_projects, vec!["project".to_string()]);
        assert_eq!(reset.favorite_models, [favorite]);
        assert!(reset.sidebar_collapsed);
        assert_eq!(
            reset.project_sort,
            tcode_core::settings::ProjectSort::NameAsc
        );
        assert_eq!(
            reset.sidebar_layout,
            tcode_core::settings::SidebarLayout::Grouped
        );
        assert_eq!(reset.last_visited.get("session"), Some(&42));
        assert_eq!(
            reset.unknown.get("future_key"),
            Some(&serde_json::json!({"kept": true})),
            "forward-compat keys must not be destroyed by a restore"
        );
    });
}

#[test]
fn provider_projection_diff_emits_once_then_suppresses_noop_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-provider-diff-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    state.deliver(
        cx,
        1000,
        tcode_protocol::ClientPayload::Subscribe(Subscription {
            topic: Topic::Providers,
            after: None,
            known_index: None,
        }),
    );
    cx.run_until_parked();
    cx.drain_outgoing();
    let command = Command::SetProfileSecret {
        profile_id: "codex".into(),
        name: "OPENAI_API_KEY".into(),
        value: Some("test-secret".into()),
    };

    state.dispatch_command(cx, 43, command.clone());
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    let provider_events: Vec<_> = outgoing
        .iter()
        .filter_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::Providers,
                event: ServerEvent::ProvidersReplaced(status),
            }) => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(provider_events.len(), 1);
    assert!(
        provider_events[0]
            .secret_names
            .get("codex")
            .is_some_and(|names| names.contains("OPENAI_API_KEY"))
    );

    state.dispatch_command(cx, 44, command);
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    assert!(!outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Providers,
            event: ServerEvent::ProvidersReplaced(_),
        })
    )));
}

/// A Claude Code stand-in that keeps its sign-in in `CLAUDE_CONFIG_DIR` and
/// answers `auth status` and, without a terminal, `auth login` the way
/// claude 2.1.314 does, and `get_usage` the way 2.1.317 does.
#[cfg(unix)]
#[test]
fn provider_login_follows_the_cli_through_sign_in_and_sign_out() {
    use std::os::unix::fs::PermissionsExt as _;
    use tcode_core::provider_status::{AuthStatus, ProviderLogin};

    let test_store = TestStore::new("tcode-provider-login-test");
    let binary = test_store.root().join("claude");
    let home = test_store.root().join("claude-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        &binary,
        "#!/bin/sh\n\
         case \"$*\" in\n\
         --version) echo '2.1.314 (Claude Code)' ;;\n\
         'auth status --json')\n\
           if [ -f \"$CLAUDE_CONFIG_DIR/signed-in\" ]; then\n\
             echo '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"email\":\"dev@example.com\"}'\n\
           else echo '{\"loggedIn\":false}'; exit 1; fi ;;\n\
         'auth login')\n\
           printf 'Opening browser to sign in\\nIf the browser did not open, visit: https://claude.com/cai/oauth/authorize?code=true&state=S\\nPaste code here if prompted > '\n\
           read code\n\
           if [ \"$code\" = good ]; then touch \"$CLAUDE_CONFIG_DIR/signed-in\"; echo 'Login successful.'; exit 0; fi\n\
           echo 'Invalid code' >&2; exit 1 ;;\n\
         'auth logout') rm -f \"$CLAUDE_CONFIG_DIR/signed-in\" ;;\n\
         '--print --input-format stream-json --output-format stream-json --verbose')\n\
           read request\n\
           if [ -f \"$CLAUDE_CONFIG_DIR/signed-in\" ]; then\n\
             echo '{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"usage-1\",\"response\":{\"subscription_type\":\"max\",\"rate_limits_available\":true,\"rate_limits\":{\"five_hour\":{\"utilization\":12,\"resets_at\":\"2026-11-04T21:20:00+00:00\"}}}}}'\n\
           else echo '{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"usage-1\",\"response\":{\"subscription_type\":null,\"rate_limits_available\":false,\"rate_limits\":null}}}'; fi ;;\n\
         *) exit 2 ;;\n\
         esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut host = TestClientState::new((*test_store).clone());
    let claude = host.settings.provider_mut(ProviderKind::ClaudeCode);
    claude.binary_path = Some(binary);
    claude.home_path = Some(home);
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(host);
    let snapshot = |state: &TestClientState, id: &str| {
        state
            .providers_status_snapshot()
            .provider_snapshots
            .get(id)
            .cloned()
            .unwrap_or_default()
    };
    let login = |state: &TestClientState| snapshot(state, "claude").login;
    let awaiting = |state: &TestClientState| {
        login(state)
            == Some(ProviderLogin::AwaitingCode {
                url: Some("https://claude.com/cai/oauth/authorize?code=true&state=S".into()),
            })
    };
    let auth = |state: &TestClientState| snapshot(state, "claude").auth.map(|auth| auth.status);

    state.dispatch_command(
        cx,
        1,
        Command::StartProviderLogin {
            profile_id: "pi".into(),
        },
    );
    cx.run_until_parked();
    assert_eq!(state.read(|state| snapshot(state, "pi").login), None);

    let start = || Command::StartProviderLogin {
        profile_id: "claude".into(),
    };
    let submit = |code: &str| Command::SubmitProviderLoginCode {
        profile_id: "claude".into(),
        code: code.into(),
    };
    state.dispatch_command(cx, 2, start());
    cx.run_until(awaiting);
    state.dispatch_command(
        cx,
        3,
        Command::CancelProviderLogin {
            profile_id: "claude".into(),
        },
    );
    cx.run_until_parked();
    assert_eq!(state.read(login), None);

    state.dispatch_command(cx, 4, start());
    cx.run_until(awaiting);
    state.dispatch_command(cx, 5, submit("bad"));
    cx.run_until(|state| {
        login(state)
            == Some(ProviderLogin::Failed {
                message: "Invalid code".into(),
            })
    });

    state.dispatch_command(cx, 6, start());
    cx.run_until(awaiting);
    state.dispatch_command(cx, 7, submit("good"));
    cx.run_until(|state| login(state).is_none() && auth(state) == Some(AuthStatus::Authenticated));
    assert_eq!(
        state.read(|state| snapshot(state, "claude").auth.unwrap().email),
        Some("dev@example.com".into())
    );
    cx.run_until(|state| {
        state
            .providers_status_snapshot()
            .provider_usage
            .get("claude")
            .is_some_and(|usage| usage.error.is_none() && usage.windows.len() == 1)
    });

    state.dispatch_command(
        cx,
        8,
        Command::LogoutProvider {
            profile_id: "claude".into(),
        },
    );
    cx.run_until(|state| auth(state) == Some(AuthStatus::Unauthenticated));
}

/// A finished turn refreshes the account usage of the profile it ran on and
/// of no other, and asks the thread's own running harness for it instead of
/// starting another. Claude Code reports `max` for the Ultra plan too, so the
/// plan is the one its account file names.
#[test]
fn a_turn_refreshes_only_its_profiles_usage_from_its_running_harness() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-turn-usage");
    let account = std::env::temp_dir().join(format!("tcode-turn-usage-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&account).unwrap();
    std::fs::write(
        account.join(".claude.json"),
        r#"{"oauthAccount":{"organizationType":"claude_max","planDisplayName":"Ultra"}}"#,
    )
    .unwrap();
    let mut client = TestClientState::new((*test_store).clone());
    client
        .settings
        .provider_mut(ProviderKind::ClaudeCode)
        .home_path = Some(account.clone());
    let state = cx.new_entity(client);
    let (commands, received) = smol::channel::unbounded();
    let session = live_session(ProviderKind::ClaudeCode, commands);
    let id = session.meta.id.clone();
    state.update(cx, |state, cx| {
        state.sessions.push(session.meta.clone());
        state.residents.live.insert(id.clone(), session);
        state.provider_event_for_test(
            &id,
            AgentEvent::TurnCompleted {
                turn_id: "turn".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
    });
    assert_eq!(
        state.read(|state| state.providers.usage_checking.clone()),
        HashSet::from(["claude".to_string()])
    );
    let reply = std::iter::from_fn(|| received.try_recv().ok())
        .find_map(|command| match command {
            SessionCommand::ReadAccountUsage { reply } => Some(reply),
            _ => None,
        })
        .expect("the running session is asked for its usage");
    reply
        .try_send(Ok(serde_json::json!({
            "subscription_type": "max",
            "rate_limits_available": true,
            "rate_limits": {"five_hour": {"utilization": 94, "resets_at": "2026-09-04T21:20:00+00:00"}}
        })))
        .unwrap();
    cx.run_until(|state| state.providers.usage_checking.is_empty());
    let usage = state.read(|state| state.providers.provider_usage.clone());
    assert_eq!(usage.keys().collect::<Vec<_>>(), ["claude"]);
    assert_eq!(usage["claude"].plan.as_deref(), Some("Ultra"));
    assert_eq!(usage["claude"].windows.len(), 1);
    std::fs::remove_dir_all(account).unwrap();
}

/// Another Claude account starts from a copy of the user's own configuration,
/// never the default account's sign-in, endpoint or history, and its sign-in
/// runs in its own config directory. A provider without account sign-in is
/// refused.
#[cfg(unix)]
#[test]
fn another_claude_account_inherits_configuration_but_not_the_account() {
    use std::os::unix::fs::PermissionsExt as _;
    use tcode_core::provider_status::ProviderLogin;

    let test_store = TestStore::new("tcode-account-profile-test");
    let binary = test_store.root().join("claude");
    let default = test_store.root().join("claude-default");
    let shared = test_store.root().join("shared");
    for (path, contents) in [
        (
            default.join("settings.json"),
            r#"{"model":"opus","apiKeyHelper":"/bin/key","permissions":{"allow":["Bash(ls)"]},"env":{"ANTHROPIC_API_KEY":"sk","ANTHROPIC_AUTH_TOKEN":"t","ANTHROPIC_BASE_URL":"https://proxy.test","FOO":"bar"}}"#,
        ),
        (default.join("commands/ship.md"), "Ship it."),
        (default.join("skills/review/SKILL.md"), "Review the diff."),
        (
            default.join("plugins/known_marketplaces.json"),
            r#"{"official":{}}"#,
        ),
        (default.join(".credentials.json"), r#"{"claudeAiOauth":{}}"#),
        (
            default.join(".claude.json"),
            r#"{"oauthAccount":{"emailAddress":"work@example.com"}}"#,
        ),
        (default.join("projects/-repo/x.jsonl"), "{}"),
        (default.join("history.jsonl"), "{}"),
        (shared.join("CLAUDE.md"), "Answer briefly."),
        (shared.join("agents/reviewer.md"), "Review."),
        (shared.join("skills/linked/SKILL.md"), "Linked."),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    // Dotfile managers link files and directories in, relative links
    // included, and a link may point back up its own tree.
    std::os::unix::fs::symlink("../shared/CLAUDE.md", default.join("CLAUDE.md")).unwrap();
    std::os::unix::fs::symlink(shared.join("agents"), default.join("agents")).unwrap();
    std::os::unix::fs::symlink("../../shared/skills/linked", default.join("skills/linked"))
        .unwrap();
    std::os::unix::fs::symlink("..", default.join("skills/review/up")).unwrap();
    std::fs::write(
        &binary,
        "#!/bin/sh\n\
         case \"$*\" in\n\
         --version) echo '2.1.314 (Claude Code)' ;;\n\
         'auth status --json') echo '{\"loggedIn\":false}'; exit 1 ;;\n\
         'auth login')\n\
           printf 'If the browser did not open, visit: https://claude.com/cai/oauth/authorize?dir=%s\\nPaste code here if prompted > ' \"$CLAUDE_CONFIG_DIR\"\n\
           read code ;;\n\
         *) exit 2 ;;\n\
         esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut host = TestClientState::new((*test_store).clone());
    let claude = host.settings.provider_mut(ProviderKind::ClaudeCode);
    claude.binary_path = Some(binary);
    claude.home_path = Some(default.clone());
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(host);
    let create = |provider| Command::CreateAccountProfile {
        provider,
        name: "Claude Personal".into(),
        inherit_config: true,
    };

    state.dispatch_command(cx, 1, create(ProviderKind::Pi));
    let (code, _) = cx.run_until_sent(|message| match message {
        HostMessage::Ack {
            id: 1,
            result: Err(error),
        } => Some(error.code.clone()),
        _ => None,
    });
    assert_eq!(code, "no_account_login");

    state.dispatch_command(cx, 2, create(ProviderKind::ClaudeCode));
    let (id, _) = cx.run_until_sent(|message| match message {
        HostMessage::Ack {
            id: 2,
            result: Ok(CommandResponse::ProfileId(id)),
        } => Some(id.clone()),
        _ => None,
    });
    let home = test_store.root().join("profile-homes").join(&id);
    let profile = state.read(|state| state.settings.resolved_profile(&id).unwrap());
    assert_eq!(profile.kind, ProviderKind::ClaudeCode);
    assert_eq!(
        profile.settings.display_name.as_deref(),
        Some("Claude Personal")
    );
    assert_eq!(profile.settings.home_path.as_ref(), Some(&home));
    assert_eq!(
        std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        r#"{"hasCompletedOnboarding":true}"#
    );
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings,
        serde_json::json!({
            "model": "opus",
            "permissions": {"allow": ["Bash(ls)"]},
            "env": {"FOO": "bar"},
        })
    );
    for (copied, contents) in [
        ("CLAUDE.md", "Answer briefly."),
        ("agents/reviewer.md", "Review."),
        ("commands/ship.md", "Ship it."),
        ("skills/review/SKILL.md", "Review the diff."),
        ("skills/linked/SKILL.md", "Linked."),
    ] {
        assert_eq!(
            std::fs::read_to_string(home.join(copied)).unwrap(),
            contents,
            "{copied}"
        );
    }
    for copy in ["CLAUDE.md", "agents", "skills/linked"] {
        assert!(
            !std::fs::symlink_metadata(home.join(copy))
                .unwrap()
                .is_symlink(),
            "{copy} must be a copy, not a link"
        );
    }
    for kept in [
        ".credentials.json",
        "projects",
        "history.jsonl",
        "plugins",
        "skills/review/up",
    ] {
        assert!(!home.join(kept).exists(), "{kept} must not be copied");
    }

    cx.run_until(|state| {
        state
            .providers_status_snapshot()
            .provider_snapshots
            .get(&id)
            .and_then(|snapshot| snapshot.login.clone())
            == Some(ProviderLogin::AwaitingCode {
                url: Some(format!(
                    "https://claude.com/cai/oauth/authorize?dir={}",
                    home.display()
                )),
            })
    });
}

/// Another Codex account gets its own `CODEX_HOME` with a copy of the user's
/// configuration, never the default account's sign-in or sessions, and its
/// sign-in is Codex's device flow run in that home: the page and the code to
/// enter there.
#[cfg(unix)]
#[test]
fn another_codex_account_signs_in_with_a_device_code_in_its_own_home() {
    use std::os::unix::fs::PermissionsExt as _;
    use tcode_core::provider_status::ProviderLogin;

    let test_store = TestStore::new("tcode-codex-account-test");
    let binary = test_store.root().join("codex");
    let default = test_store.root().join("codex-default");
    for (path, contents) in [
        (default.join("config.toml"), "model = \"gpt-6\"\n"),
        (default.join("AGENTS.md"), "Answer briefly."),
        (default.join("auth.json"), r#"{"tokens":{}}"#),
        (default.join("sessions/rollout.jsonl"), "{}"),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    // What codex-cli 0.166.0 prints for `login --device-auth`, colored.
    std::fs::write(
        &binary,
        "#!/bin/sh\n\
         case \"$*\" in\n\
         --version) echo 'codex-cli 0.166.0' ;;\n\
         'login --device-auth')\n\
           printf '1. Open this link in your browser\\n   \\033[94mhttps://auth.openai.com/codex/device?home=%s\\033[0m\\n\\n2. Enter this one-time code\\n   \\033[94m3JP6-MXLMX\\033[0m\\n' \"$CODEX_HOME\"\n\
           read code ;;\n\
         *) exit 2 ;;\n\
         esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut host = TestClientState::new((*test_store).clone());
    let codex = host.settings.provider_mut(ProviderKind::Codex);
    codex.binary_path = Some(binary);
    codex.home_path = Some(default.clone());
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(host);
    state.dispatch_command(
        cx,
        1,
        Command::CreateAccountProfile {
            provider: ProviderKind::Codex,
            name: "Codex Work".into(),
            inherit_config: true,
        },
    );
    let (id, _) = cx.run_until_sent(|message| match message {
        HostMessage::Ack {
            id: 1,
            result: Ok(CommandResponse::ProfileId(id)),
        } => Some(id.clone()),
        _ => None,
    });
    let home = test_store.root().join("profile-homes").join(&id);
    assert_eq!(
        state.read(|state| state
            .settings
            .resolved_profile(&id)
            .unwrap()
            .settings
            .home_path),
        Some(home.clone())
    );
    for (copied, contents) in [
        ("config.toml", "model = \"gpt-6\"\n"),
        ("AGENTS.md", "Answer briefly."),
    ] {
        assert_eq!(
            std::fs::read_to_string(home.join(copied)).unwrap(),
            contents
        );
    }
    for kept in ["auth.json", "sessions"] {
        assert!(!home.join(kept).exists(), "{kept} must not be copied");
    }
    cx.run_until(|state| {
        state
            .providers_status_snapshot()
            .provider_snapshots
            .get(&id)
            .and_then(|snapshot| snapshot.login.clone())
            == Some(ProviderLogin::AwaitingDevice {
                url: Some(format!(
                    "https://auth.openai.com/codex/device?home={}",
                    home.display()
                )),
                code: "3JP6-MXLMX".into(),
            })
    });
}

/// An endpoint profile that has no API key saved says so, rather than
/// passing on the CLI's advice to sign in, which does not apply to it; the
/// account beside it is told to sign in.
#[cfg(unix)]
#[test]
fn an_endpoint_profile_without_its_key_is_missing_the_key_not_signed_out() {
    use std::os::unix::fs::PermissionsExt as _;
    use tcode_core::provider_status::ProviderProbeDiagnostic;

    let test_store = TestStore::new("tcode-endpoint-key-test");
    let binary = test_store.root().join("claude");
    std::fs::write(
        &binary,
        "#!/bin/sh\n\
         case \"$*\" in\n\
         --version) echo '2.1.318 (Claude Code)' ;;\n\
         'auth status --json') echo '{\"loggedIn\":false}'; exit 1 ;;\n\
         *) exit 2 ;;\n\
         esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut host = TestClientState::new((*test_store).clone());
    host.settings
        .provider_mut(ProviderKind::ClaudeCode)
        .binary_path = Some(binary);
    host.settings.profiles.insert(
        "kimi".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                env: vec![
                    EnvVar {
                        name: "ANTHROPIC_BASE_URL".into(),
                        value: "https://api.kimi.com/coding/".into(),
                        sensitive: false,
                    },
                    EnvVar {
                        name: "ANTHROPIC_API_KEY".into(),
                        value: String::new(),
                        sensitive: true,
                    },
                ],
                ..ProviderSettings::default()
            },
        },
    );
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(host);
    state.update(cx, |state, cx| state.refresh_provider_status(cx));
    let diagnostic = |state: &TestClientState, id: &str| {
        state
            .providers_status_snapshot()
            .provider_snapshots
            .get(id)
            .filter(|snapshot| !snapshot.checking)
            .and_then(|snapshot| snapshot.diagnostic)
    };
    cx.run_until(|state| {
        diagnostic(state, "kimi").is_some() && diagnostic(state, "claude").is_some()
    });
    state.read(|state| {
        assert_eq!(
            diagnostic(state, "kimi"),
            Some(ProviderProbeDiagnostic::MissingApiKey)
        );
        assert_eq!(
            diagnostic(state, "claude"),
            Some(ProviderProbeDiagnostic::Unauthenticated)
        );
    });
}

/// Each settings edit writes the one field it names: the harness's own
/// fields on its built-in profile, where an account without its own follows
/// them, and never the config directory an account's sign-in lives in.
#[test]
fn a_settings_edit_writes_the_field_it_names() {
    let test_store = TestStore::new("tcode-profile-patch-test");
    let mut host = TestClientState::new((*test_store).clone());
    let home = test_store.root().join("profile-homes").join("work");
    host.settings.profiles.insert(
        "work".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                home_path: Some(home.clone()),
                ..ProviderSettings::default()
            },
        },
    );
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(host);
    let var = |name: &str, value: &str| EnvVar {
        name: name.into(),
        value: value.into(),
        sensitive: false,
    };
    for (id, (profile, patch)) in [
        (
            "claude",
            ProfileSettingsPatch::SetBinaryPath(Some("/opt/claude".into())),
        ),
        (
            "work",
            ProfileSettingsPatch::SetHomePath(Some("/elsewhere".into())),
        ),
        (
            "claude",
            ProfileSettingsPatch::SetHomePath(Some("/home/.claude".into())),
        ),
        (
            "work",
            ProfileSettingsPatch::SetSharedEnv(vec![var("A", "1")]),
        ),
        (
            "claude",
            ProfileSettingsPatch::SetSharedEnv(vec![var(" A ", "1"), var("", "x")]),
        ),
        (
            "work",
            ProfileSettingsPatch::SetDisplayName(Some("  Office ".into())),
        ),
        (
            "pi",
            ProfileSettingsPatch::SetToggle {
                id: "trust_project_extensions".into(),
                enabled: true,
            },
        ),
        (
            "claude",
            ProfileSettingsPatch::SetHarnessEnabled { enabled: false },
        ),
    ]
    .into_iter()
    .enumerate()
    {
        state.dispatch_command(
            cx,
            id as u64 + 1,
            Command::UpdateProfileSettings {
                profile_id: profile.into(),
                patch,
            },
        );
    }
    cx.run_until_parked();
    state.read(|state| {
        let claude = state.settings.provider(ProviderKind::ClaudeCode);
        let work = state.settings.resolved_profile("work").unwrap();
        assert_eq!(claude.binary_path, Some(PathBuf::from("/opt/claude")));
        assert_eq!(
            work.settings.binary_path,
            Some(PathBuf::from("/opt/claude"))
        );
        assert_eq!(state.settings.profiles["work"].settings.binary_path, None);
        assert_eq!(work.settings.home_path, Some(home.clone()));
        assert_eq!(claude.home_path, Some(PathBuf::from("/home/.claude")));
        assert_eq!(claude.shared_env, vec![var("A", "1")]);
        assert_eq!(work.shared_env, vec![var("A", "1")]);
        assert!(work.settings.shared_env.is_empty());
        assert_eq!(work.settings.display_name.as_deref(), Some("Office"));
        assert!(
            state
                .settings
                .provider(ProviderKind::Pi)
                .pi
                .trust_project_extensions
        );
        assert!(!claude.enabled && !work.settings.enabled);
        assert!(state.settings.provider(ProviderKind::Codex).enabled);
    });
}

#[test]
fn scripted_provider_connects_command_launch_and_agent_event_paths() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scripted-provider-seam-test");
    let cwd = test_store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let project = Project::from_root(cwd.clone());
    test_store.upsert_project(&project).unwrap();
    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let commands = scripted.commands.clone();
    let events = scripted.events.clone();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher_for_test(scripted.launcher);
        state
    });
    state.dispatch_command(
        cx,
        51,
        Command::StartDraft {
            project_id: project.id.clone(),
            cwd: cwd.clone(),
        },
    );
    let session_id = state.read(|state| state.residents.live.keys().next().unwrap().clone());
    state.update(cx, |state, cx| state.select_session(&session_id, cx));
    for (id, topic) in [
        (
            53,
            Topic::SessionStatus {
                session_id: session_id.clone(),
            },
        ),
        (
            54,
            Topic::SessionEvents {
                session_id: session_id.clone(),
            },
        ),
    ] {
        state.deliver(
            cx,
            id,
            tcode_protocol::ClientPayload::Subscribe(Subscription {
                topic,
                after: None,
                known_index: None,
            }),
        );
    }

    cx.run_until_parked();
    assert!(cx.drain_outgoing().iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            request_id: Some(53),
            topic: Topic::SessionStatus { session_id: emitted_id },
            event: ServerEvent::SessionStatusReplaced(status),
            ..
        }) if emitted_id == &session_id
            && status.session_id == session_id
            && status.project_id.as_deref() == Some(project.id.as_str())
            && status.cwd == cwd
            && status.draft
    )));
    state.read(|state| {
        let draft = state.selected_session().unwrap();
        assert!(draft.draft);
        assert!(matches!(draft.runtime, Runtime::Idle));
        assert!(!state.sessions.iter().any(|meta| meta.id == session_id));
        assert!(
            !state
                .store
                .load_index()
                .unwrap()
                .iter()
                .any(|meta| meta.id == session_id)
        );
    });
    state.dispatch_command(
        cx,
        52,
        Command::SendTurn {
            session_id: session_id.clone(),
            text: "exercise the adapter".into(),
            attachment_paths: Vec::new(),
        },
    );
    cx.run_until(|_| !commands.is_empty());

    let delivery_id = match commands.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) => {
            assert_eq!(text, "exercise the adapter");
            delivery_id
        }
        other => panic!("expected scripted provider SendTurn, got {other:?}"),
    };
    events
        .try_send(AgentEvent::TurnAccepted { delivery_id })
        .unwrap();
    events
        .try_send(AgentEvent::TurnStarted {
            turn_id: "scripted-turn".into(),
        })
        .unwrap();
    cx.run_until(|state| {
        state.selected_session().is_some_and(|session| {
            session
                .timeline
                .turns
                .iter()
                .any(|turn| turn.provider_turn_id.as_deref() == Some("scripted-turn"))
        })
    });

    // The subscriber hears of the turn once the log its subscription waits
    // on is read, which happens off the host.
    cx.run_until_sent(|message| {
        matches!(
            message,
            HostMessage::Event(EventEnvelope { request_id: None,
                topic: Topic::SessionEvents { .. },
                event: ServerEvent::SessionEvent(SessionEventRecord {
                    event: AgentEvent::TurnStarted { turn_id },
                    ..
                }),
                ..
            }) if turn_id == "scripted-turn"
        )
        .then_some(())
    });
    state.read(|state| {
        let session = state.selected_session().unwrap();
        assert!(session.turn_in_flight);
        assert!(!session.draft);
        for meta in [
            state
                .sessions
                .iter()
                .find(|meta| meta.id == session_id)
                .unwrap()
                .clone(),
            state
                .store
                .load_index()
                .unwrap()
                .into_iter()
                .find(|meta| meta.id == session_id)
                .unwrap(),
        ] {
            assert_eq!(meta.cwd, cwd);
            assert_eq!(meta.project_id.as_deref(), Some(project.id.as_str()));
        }
    });
}

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

#[test]
fn deleting_a_thread_deletes_every_descendant_in_one_command() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-delete-cascade-test");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    for (id, parent) in [
        ("parent", None),
        ("child", Some("parent")),
        ("grandchild", Some("child")),
        ("other", None),
    ] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_string);
        if id == "child" {
            meta.archived_at = Some(1);
        }
        store.upsert_meta(&meta).unwrap();
        store
            .append_event(id, 1, &AgentEvent::Warning { message: id.into() })
            .unwrap();
    }
    let state = cx.new_entity(TestClientState::new(store.clone()));
    let (commands, background) = smol::channel::unbounded();
    state.update(cx, |state, _| {
        let mut grandchild = live_session(ProviderKind::Codex, commands);
        grandchild.meta = state.find_meta("grandchild").unwrap();
        state
            .residents
            .parked
            .insert("grandchild".into(), grandchild);
    });

    state.dispatch_command(
        cx,
        1,
        Command::DeleteSession {
            session_id: "parent".into(),
            remove_worktree: false,
        },
    );
    cx.run_until_parked();

    assert!(matches!(
        background.try_recv(),
        Ok(SessionCommand::Shutdown)
    ));
    state.update(cx, |state, _| state.close_store().unwrap());
    let reopened = SessionStore::open_at(root).unwrap();
    let remaining: Vec<_> = reopened
        .load_index()
        .unwrap()
        .into_iter()
        .map(|meta| meta.id)
        .collect();
    assert_eq!(remaining, ["other"]);
    for id in ["parent", "child", "grandchild"] {
        assert!(reopened.read_event_log(id).unwrap().is_empty(), "{id}");
    }
    assert!(!reopened.read_event_log("other").unwrap().is_empty());
    reopened.close().unwrap();
}

#[test]
fn title_session_uses_configured_model_with_low_effort() {
    let defaults = title_session_meta(&Settings::default(), PathBuf::from("/tmp/project"));
    assert_eq!(defaults.provider, ProviderKind::Codex);
    assert_eq!(defaults.model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(
        defaults.option_selections,
        vec![OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("low"),
        }]
    );

    let mut settings = Settings::default();
    settings.title_generation.provider = ProviderKind::ClaudeCode;
    settings.title_generation.model = "claude-haiku-4-5".into();
    settings.profiles.insert(
        "work-claude".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings::default(),
        },
    );
    settings.title_generation.profile_id = Some("work-claude".into());
    let custom = title_session_meta(&settings, PathBuf::from("/tmp/project"));
    assert_eq!(custom.provider, ProviderKind::ClaudeCode);
    assert_eq!(custom.model.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(custom.profile_id.as_deref(), Some("work-claude"));
    assert_eq!(
        title_turn_options().effort.as_deref(),
        Some(AI_TITLE_REASONING_EFFORT)
    );

    settings.title_generation.profile_id = Some("deleted-profile".into());
    let fallback = title_session_meta(&settings, PathBuf::from("/tmp/project"));
    assert_eq!(fallback.profile_id, None);
}

#[test]
fn title_regeneration_uses_stored_history_and_preserves_intervening_changes() {
    for (outcome, raw_title, status, output_tokens, expected) in [
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
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-regenerate-title");
        let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
        meta.title = "Old title".into();
        meta.updated_at = 123;
        let id = meta.id.clone();
        store.upsert_meta(&meta).unwrap();
        let image = store.root().join("qr.png");
        fs::write(&image, [1, 2, 3]).unwrap();
        for (item_id, content) in [
            (
                "user",
                ItemContent::UserMessage {
                    text: "hidden instructions\nImprove QR sharing".into(),
                    context_len: Some("hidden instructions\n".len()),
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
                    &id,
                    1,
                    &AgentEvent::ItemCompleted(ThreadItem {
                        id: item_id.into(),
                        parent_item_id: None,
                        content,
                    }),
                )
                .unwrap();
        }
        let scripted = scripted_provider(ProviderKind::Codex);
        let state = cx.new_entity({
            let mut state = TestClientState::new((*store).clone());
            state.ai_title_generation_enabled = true;
            state.set_provider_launcher_for_test(scripted.launcher);
            state
        });
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
        if outcome == "cached" {
            state.update(cx, |state, cx| {
                state.record_event(
                    &id,
                    &AgentEvent::ItemCompleted(ThreadItem {
                        id: "latest".into(),
                        parent_item_id: None,
                        content: ItemContent::UserMessage {
                            text: "Include link expiration".into(),
                            context_len: None,
                            attachments: Vec::new(),
                        },
                    }),
                    cx,
                );
            });
        }
        // No selected or resident session. The cached case also covers an accepted
        // message captured before its queued disk write has completed.
        for request_id in [1, 2] {
            state.dispatch_command(
                cx,
                request_id,
                Command::RegenerateSessionTitle {
                    session_id: id.clone(),
                },
            );
        }
        cx.run_until(|_| !scripted.commands.is_empty());
        let prompt = match scripted.commands.try_recv().unwrap() {
            SessionCommand::SendTurn {
                text, attachments, ..
            } => {
                assert_eq!(attachments.len(), 1);
                assert_eq!(attachments[0].data_base64, "AQID");
                text
            }
            other => panic!("expected a title request, got {other:?}"),
        };
        assert!(
            scripted.commands.try_recv().is_err(),
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
        cx.run_until(|state| {
            !state
                .index_snapshot()
                .summary
                .title_generating
                .contains(&id)
        });
        state.read(|state| {
            let title = state
                .sessions
                .iter()
                .find(|meta| meta.id == id)
                .map(|meta| meta.title.as_str());
            assert_eq!(title, expected, "{outcome}");
            let persisted = store
                .load_index()
                .unwrap()
                .into_iter()
                .find(|meta| meta.id == id);
            assert_eq!(persisted.as_ref().map(|meta| meta.title.as_str()), expected);
            if !matches!(outcome, "renamed" | "deleted") {
                assert_eq!(state.sessions[0].updated_at, 123, "{outcome}");
                assert_eq!(persisted.unwrap().updated_at, 123, "{outcome}");
            }
        });
        if outcome == "failed" {
            assert!(cx.drain_outgoing().iter().any(|message| matches!(
                message,
                HostMessage::Event(EventEnvelope {
                    event: ServerEvent::Runtime(RuntimeEvent::Error(
                        RuntimeError::TitleGenerationFailed
                    )),
                    ..
                })
            )));
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
        state.set_provider_launcher_for_test(scripted.launcher);
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
    let first = format!("hidden\nOriginal QR sharing goal {}", "界".repeat(3_000));
    let timeline = Timeline::fold_events([
        item(
            "first",
            ItemContent::UserMessage {
                text: first,
                context_len: Some("hidden\n".len()),
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
                context_len: None,
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
    assert!(context.contains("[Earlier content truncated]"));
    assert!(context.contains("USER:\nFocus on QR expiration"));
    assert!(context.ends_with("ASSISTANT:\nMake shared QR links last longer"));
    assert!(!context.contains("hidden"));
    assert!(!context.contains("private reasoning"));
    assert!(!context.contains("tool output"));
    assert!(context.chars().count() <= 8_000);
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

#[test]
fn installed_acp_duplicates_of_native_providers_only_serve_their_existing_sessions() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-native-acp-duplicates");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let installed = |id: &str| InstalledAgent {
        id: id.into(),
        name: id.into(),
        version: String::new(),
        icon: None,
        launch: agent::AcpLaunch::Custom {
            command: id.into(),
            args: Vec::new(),
            env: Vec::new(),
        },
        enabled: true,
        env: Vec::new(),
        launch_args: None,
    };
    let draft = state.update(cx, |state, cx| {
        for id in ["cursor", "grok-build", "gemini"] {
            state.settings.acp_agents.insert(id.into(), installed(id));
        }
        state.start_draft("project".into(), PathBuf::from("/tmp/project"), cx);
        state.selected.clone().unwrap()
    });
    for (request, id, selected) in [
        (1, "cursor", None),
        (2, "grok-build", None),
        (3, "gemini", Some("gemini")),
    ] {
        state.dispatch_command(
            cx,
            request,
            Command::SetActiveAcpAgent {
                session_id: draft.clone(),
                id: id.into(),
            },
        );
        state.read(|state| {
            assert_eq!(
                state.resident(&draft).unwrap().meta.acp_agent_id.as_deref(),
                selected,
                "{id}"
            );
        });
    }

    let mut existing = SessionMeta::new(ProviderKind::Acp, PathBuf::from("/tmp/project"), None);
    existing.acp_agent_id = Some("cursor".into());
    state.read(|state| {
        let opts = session_options(
            &existing,
            &state.settings,
            LaunchEnv::default(),
            McpServers::default(),
        );
        assert_eq!(opts.acp.map(|agent| agent.id).as_deref(), Some("cursor"));
    });
}

#[test]
fn marketplace_items_are_runtime_owned_views() {
    let test_store = TestStore::new("tcode-marketplace-view-test");
    let store = (*test_store).clone();
    let mut state = TestClientState::new(store);
    *state.acp_registry = Some(
        serde_json::from_value(serde_json::json!({
            "agents": [
                {
                    "id": "first",
                    "name": "First",
                    "version": "1.2.3",
                    "description": "Supported agent",
                    "distribution": { "npx": { "package": "first-agent" } }
                },
                {
                    "id": "claude-acp",
                    "name": "Hidden",
                    "distribution": { "npx": { "package": "hidden-agent" } }
                },
                {
                    "id": "last",
                    "name": "Last",
                    "version": "4.5.6",
                    "description": "Unsupported agent",
                    "distribution": {}
                }
            ]
        }))
        .unwrap(),
    );
    state.settings.acp_agents.insert(
        "first".into(),
        InstalledAgent {
            id: "first".into(),
            name: "First".into(),
            version: "1.2.3".into(),
            icon: None,
            launch: agent::AcpLaunch::Npx {
                package: "first-agent".into(),
                args: Vec::new(),
                env: Vec::new(),
            },
            enabled: true,
            env: Vec::new(),
            launch_args: None,
        },
    );
    state.acp_installing.insert("last".into());

    assert_eq!(
        state.acp_marketplace_items(),
        vec![
            AcpMarketplaceItem {
                id: "first".into(),
                name: "First".into(),
                version: "1.2.3".into(),
                description: "Supported agent".into(),
                installed: true,
                installing: false,
                supported: true,
            },
            AcpMarketplaceItem {
                id: "last".into(),
                name: "Last".into(),
                version: "4.5.6".into(),
                description: "Unsupported agent".into(),
                installed: false,
                installing: true,
                supported: false,
            },
        ]
    );
}

/// A directory already in `profile-homes` — the config directory of an
/// account whose profile the settings no longer hold — is that account's: a
/// new profile of the same name gets a directory of its own, and deleting the
/// new profile leaves the old directory as it was.
#[test]
fn a_new_profile_never_takes_over_a_directory_left_in_profile_homes() {
    let store = TestStore::new("tcode-profile-home-left");
    let left = store.root().join("profile-homes").join("work");
    let account = r#"{"oauthAccount":{"emailAddress":"work@example.com"}}"#;
    std::fs::create_dir_all(left.join("projects/-repo")).unwrap();
    std::fs::write(left.join(".claude.json"), account).unwrap();
    std::fs::write(left.join("projects/-repo/session.jsonl"), "{}").unwrap();
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));

    state.dispatch_command(
        cx,
        1,
        Command::CreateThirdPartyProfile {
            provider: ProviderKind::ClaudeCode,
            name: "Work".into(),
            base_url: "https://proxy.test".into(),
            model: None,
            api_key: "sk-test".into(),
            inherit_config: false,
        },
    );
    cx.run_until(|state| !state.settings.profiles.is_empty());
    let (id, home) = state.read(|state| {
        let (id, profile) = state.settings.profiles.iter().next().unwrap();
        (id.clone(), profile.settings.home_path.clone().unwrap())
    });
    assert_ne!(home, left);
    assert_eq!(home, store.root().join("profile-homes").join(&id));
    assert!(home.join(".claude.json").is_file());

    state.update(cx, |state, cx| state.delete_profile(&id, cx));
    // The directory is removed off the host.
    cx.run_until(|_| !home.exists());
    assert_eq!(
        std::fs::read_to_string(left.join(".claude.json")).unwrap(),
        account
    );
    assert!(left.join("projects/-repo/session.jsonl").is_file());
}

/// A settings file this build cannot read — one a newer build wrote, opened
/// by an older install on the same machine — is never written over: the
/// host runs on defaults, the first write it refuses says why, and the file
/// stays for the build that reads it.
#[test]
fn an_unreadable_settings_file_is_never_written_over() {
    let store = TestStore::new("tcode-unreadable-settings");
    let path = store.root().join("settings.json");
    // A theme a newer build added.
    let newer = r#"{"profiles":{"work":{"kind":"claude_code","display_name":"Work"}},"theme_mode":"sepia"}"#;
    std::fs::write(&path, newer).unwrap();
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    assert!(state.read(|state| state.settings.profiles.is_empty()));

    for (id, wrap) in [(1, true), (2, false)] {
        state.dispatch_command(
            cx,
            id,
            Command::PatchSettings {
                patch: SettingsPatch::WordWrapDiffs(wrap),
            },
        );
    }
    cx.run_until_parked();
    let refusals = cx
        .drain_outgoing()
        .into_iter()
        .filter(|message| {
            matches!(
                message,
                HostMessage::Event(EventEnvelope {
                    event: ServerEvent::Runtime(RuntimeEvent::Error(
                        RuntimeError::SettingsUnreadable { .. }
                    )),
                    ..
                })
            )
        })
        .count();
    assert_eq!(refusals, 1);
    state.update(cx, |state, _| state.close_store().unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
}

/// A deleted profile's id comes back with the next profile of that name, which
/// owns none of its stars; other accounts' stars and bare ids stay.
#[test]
fn deleting_a_profile_takes_its_stars_with_it() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-delete-profile-stars");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        state.settings.profiles.insert(
            "second-account".into(),
            ProviderProfile {
                kind: ProviderKind::Codex,
                settings: Default::default(),
            },
        );
        for profile_id in [Some("second-account".to_owned()), None] {
            state.toggle_favorite_model(ProviderKind::Codex, profile_id, "model".into(), cx);
        }
        state
            .settings
            .favorite_models
            .push(FavoriteModel::Id("older".into()));

        state.delete_profile("second-account", cx);

        assert_eq!(
            state.settings.favorite_models,
            [
                FavoriteModel::Row {
                    provider: ProviderKind::Codex,
                    profile_id: None,
                    model: "model".into(),
                },
                FavoriteModel::Id("older".into()),
            ]
        );
    });
}

#[test]
fn send_turn_assembles_draft_context_and_attachment_paths() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-send-assembly-test");
    let root = test_store.root().clone();
    std::fs::create_dir_all(&root).unwrap();
    let attachment_path = root.join("sample.png");
    std::fs::write(&attachment_path, [1, 2, 3]).unwrap();
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "assembled".into();
        active.terminal_workspace.contexts.push(TerminalContext {
            id: 1,
            terminal_label: "zsh".into(),
            line_start: 12,
            line_end: 13,
            text: "cargo test\nok".into(),
        });
        state.install_selected(active);
        state.add_review_comment("assembled",
            ReviewComment::new(
                "src/lib.rs".into(),
                7,
                7,
                tcode_core::session::ReviewSide::New,
                "Please fix".into(),
                "let bad = true;".into(),
                "section".into(),
                "Changes".into(),
                3,
                4,
            ),
            cx,
        );

        state.add_review_comment(
            "assembled",
            ReviewComment::new(
                "src/lib.rs".into(),
                7,
                8,
                tcode_core::session::ReviewSide::New,
                "  Please avoid the unwrap.  ".into(),
                "@@ -7,1 +7,2 @@\n old\n+new".into(),
                "turn:3".into(),
                "Turn 4".into(),
                12,
                13,
            ),
            cx,
        );
        state.send_turn("assembled", "Explain this".into(), vec![attachment_path.clone()], cx);

        let SessionCommand::SendTurn {
            text, attachments, ..
        } = receiver.try_recv().expect("assembled send command")
        else {
            panic!("expected SendTurn")
        };
        assert_eq!(
            text,
            "Explain this\n\n<terminal_context>\n- zsh lines 12-13:\n  12 | cargo test\n  13 | ok\n</terminal_context>\n\n<review_comment sectionId=\"section\" sectionTitle=\"Changes\" filePath=\"src/lib.rs\" startIndex=\"3\" endIndex=\"4\" rangeLabel=\"+7\">\nPlease fix\n```diff\nlet bad = true;\n```\n</review_comment>\n\n<review_comment sectionId=\"turn:3\" sectionTitle=\"Turn 4\" filePath=\"src/lib.rs\" startIndex=\"12\" endIndex=\"13\" rangeLabel=\"+7 to +8\">\nPlease avoid the unwrap.\n```diff\n@@ -7,1 +7,2 @@\n old\n+new\n```\n</review_comment>"
        );
        assert_eq!(
            attachments,
            vec![Attachment {
                media_type: "image/png".into(),
                data_base64: "AQID".into(),
                source_path: Some(attachment_path.to_string_lossy().into_owned()),
            }]
        );
        assert!(
            state.selected_session()
                .unwrap()
                .terminal_workspace
                .contexts
                .is_empty()
        );
        assert!(state.review_comments("assembled").is_empty());
    });
}

/// A remote client on a bad network can queue a thread's subscription and its
/// unsubscription together and deliver both after it reconnects, although the
/// conversation never loaded on its screen. Only the client's acknowledgement
/// that it showed the thread marks it read.
#[test]
fn only_a_loaded_conversation_marks_a_thread_read() {
    use tcode_protocol::{ClientPayload, Subscription, Topic};
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-read-on-load-test");
    let store = (*test_store).clone();
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/a"), None);
    meta.updated_at = 100;
    store.upsert_meta(&meta).unwrap();
    let id = meta.id.clone();
    let state = cx.new_entity(TestClientState::new(store));
    state.update(cx, |state, _| {
        state.settings.last_visited.insert(id.clone(), 50);
    });
    // Read through the index projection every client shows.
    let unread = |state: &TestEntity| {
        state.read(|state| state.index_snapshot().summary.activity[&id].unread)
    };
    assert!(unread(&state), "updated after the last visit");
    state.read(|state| {
        let activity = &state.index_snapshot().summary.activity[&id];
        assert!(
            !activity.working
                && !activity.turn_running
                && activity.background.is_idle()
                && activity.unsettled == 0
                && !activity.waiting_for_approval
                && !activity.waiting_for_input,
            "a thread with no live provider has no work"
        );
    });

    let subscription = Subscription {
        known_index: None,
        topic: Topic::SessionEvents {
            session_id: id.clone(),
        },
        after: None,
    };
    state.deliver(cx, 1, ClientPayload::Subscribe(subscription.clone()));
    state.deliver(cx, 2, ClientPayload::Unsubscribe(subscription));
    cx.run_until_parked();
    assert!(unread(&state), "a late subscription is not a read");

    let read = |through| Command::MarkSessionRead {
        session_id: id.clone(),
        through,
    };
    state.dispatch_command(cx, 3, read(100));
    assert!(!unread(&state), "read through what the client showed");
    state.dispatch_command(cx, 4, read(80));
    assert!(
        !unread(&state),
        "a delayed older acknowledgement does not rewind"
    );

    state.update(cx, |state, cx| {
        let mut parked = state.sessions.iter().find(|m| m.id == id).cloned().unwrap();
        parked.updated_at = 120;
        state.persist_meta(&parked, cx);
    });
    assert!(unread(&state), "newer than anything the client showed");

    state.update(cx, |state, cx| state.settle_session(&id, cx));
    assert!(!unread(&state), "a settled thread is never unread");
    state.update(cx, |state, cx| state.unsettle_session(&id, cx));
    assert!(
        unread(&state),
        "unsettling restores what the client never saw"
    );
}

#[test]
fn draft_defaults_follow_project_history_then_global_history_without_persisting_a_draft() {
    let history = |provider, project: &str, model: &str, updated_at| {
        let mut meta = SessionMeta::new(provider, PathBuf::from("/history"), Some(model.into()));
        meta.project_id = Some(project.into());
        meta.updated_at = updated_at;
        meta.option_selections = vec![
            OptionSelection {
                id: "serviceTier".into(),
                value: serde_json::json!("fast"),
            },
            OptionSelection {
                id: "reasoningEffort".into(),
                value: serde_json::json!("high"),
            },
        ];
        meta
    };
    let older = history(ProviderKind::ClaudeCode, "target", "sonnet", 100);
    let mut archived = history(ProviderKind::Codex, "target", "archived", 1000);
    archived.archived_at = Some(1001);
    let mut global = history(ProviderKind::Acp, "other", "global-model", 900);
    global.acp_agent_id = Some("global-agent".into());
    let mut acp = history(ProviderKind::Acp, "target", "agent-model", 500);
    acp.acp_agent_id = Some("agent.example".into());
    let mut profile = history(ProviderKind::ClaudeCode, "target", "k3[1m]", 500);
    profile.profile_id = Some("klaude-kode".into());
    let codex = history(ProviderKind::Codex, "target", "gpt-5.2-codex", 500);
    for (name, sessions, provider, model, agent, profile, effort) in [
        (
            "project",
            vec![global.clone(), older.clone(), archived.clone(), codex],
            ProviderKind::Codex,
            Some("gpt-5.2-codex"),
            None,
            None,
            true,
        ),
        (
            "acp",
            vec![global.clone(), archived.clone(), acp],
            ProviderKind::Acp,
            Some("agent-model"),
            Some("agent.example"),
            None,
            true,
        ),
        (
            "profile",
            vec![global.clone(), older, profile],
            ProviderKind::ClaudeCode,
            Some("k3[1m]"),
            None,
            Some("klaude-kode"),
            true,
        ),
        (
            "global",
            vec![global.clone()],
            ProviderKind::Acp,
            Some("global-model"),
            Some("global-agent"),
            None,
            false,
        ),
        (
            "archived target",
            vec![archived.clone(), global],
            ProviderKind::Acp,
            Some("global-model"),
            Some("global-agent"),
            None,
            false,
        ),
        (
            "all archived",
            vec![archived],
            ProviderKind::ClaudeCode,
            None,
            None,
            None,
            false,
        ),
        (
            "empty",
            vec![],
            ProviderKind::ClaudeCode,
            None,
            None,
            None,
            false,
        ),
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-draft-defaults");
        let state = cx.new_entity(TestClientState::new((*store).clone()));
        state.update(cx, |state, cx| {
            state.sessions = sessions.into();
            state.start_draft("target".into(), PathBuf::from("/new-project"), cx);
            let draft = state.selected_session().unwrap();
            assert!(draft.draft, "{name}");
            assert_eq!(draft.meta.provider, provider, "{name}");
            assert_eq!(draft.meta.model.as_deref(), model, "{name}");
            assert_eq!(draft.meta.acp_agent_id.as_deref(), agent, "{name}");
            assert_eq!(draft.meta.profile_id.as_deref(), profile, "{name}");
            assert_eq!(draft.meta.cwd, PathBuf::from("/new-project"));
            assert_eq!(draft.meta.project_id.as_deref(), Some("target"));
            let mut expected = if effort {
                vec![OptionSelection {
                    id: "reasoningEffort".into(),
                    value: serde_json::json!("high"),
                }]
            } else {
                vec![]
            };
            expected.extend(
                match provider {
                    ProviderKind::ClaudeCode => Some(("permissionMode", "auto")),
                    ProviderKind::Codex => Some(("permissions", "auto_review")),
                    _ => None,
                }
                .map(|(id, value)| OptionSelection {
                    id: id.into(),
                    value: serde_json::json!(value),
                }),
            );
            assert_eq!(draft.meta.option_selections, expected, "{name}");
            assert!(state.store.load_index().unwrap().is_empty(), "{name}");
        });
    }
}

#[test]
fn draft_model_selection_switches_to_the_rows_explicit_provider() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-draft-provider-selection-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        let mut previous = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/previous"),
            Some("gpt-5.6-sol".into()),
        );
        previous.project_id = Some("project-target".into());
        previous.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("high"),
        });
        state.sessions = vec![previous].into();
        state.start_draft("project-target".into(), PathBuf::from("/tmp/target"), cx);

        let draft = state.selected_session().unwrap();
        assert_eq!(draft.meta.provider, ProviderKind::Codex);
        assert_eq!(draft.meta.model.as_deref(), Some("gpt-5.6-sol"));

        // `claude-fable-5-1` cannot be reliably classified by a hard-coded
        // model-name heuristic. The provider comes from its picker row.
        state.host.set_active_model(
            state.selected.as_deref().unwrap_or_default(),
            ProviderKind::ClaudeCode,
            Some("claude-fable-5-1".into()),
            None,
            cx,
        );

        let draft = state.selected_session().unwrap();
        assert_eq!(draft.meta.provider, ProviderKind::ClaudeCode);
        assert_eq!(draft.meta.model.as_deref(), Some("claude-fable-5-1"));
        assert!(draft.meta.acp_agent_id.is_none());
        assert_eq!(
            draft.meta.option_selections,
            vec![OptionSelection {
                id: "permissionMode".into(),
                value: serde_json::json!("auto"),
            }]
        );
        assert!(state.store.load_index().unwrap().is_empty());
    });
}

#[test]
fn model_switch_restores_last_effort_used_with_that_model() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-model-switch-effort-memory-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        let mut sol = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/sol"),
            Some("gpt-5.6-sol".into()),
        );
        sol.project_id = Some("project-target".into());
        sol.updated_at = 10;
        sol.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("max"),
        });
        let mut fable = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/fable"),
            Some("claude-fable-5-1".into()),
        );
        fable.project_id = Some("project-target".into());
        fable.updated_at = 20;
        // The same model last ran on another account, at an effort of its own.
        let mut other_account = sol.clone();
        other_account.profile_id = Some("second-account".into());
        other_account.updated_at = 15;
        other_account.option_selections = vec![OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("low"),
        }];
        state.sessions = vec![sol, other_account, fable].into();
        state.start_draft("project-target".into(), PathBuf::from("/tmp/target"), cx);

        // Switching the draft to a model brings back the effort it last ran at
        // on that account, not the model's default.
        state.host.set_active_model(
            state.selected.as_deref().unwrap_or_default(),
            ProviderKind::Codex,
            Some("gpt-5.6-sol".into()),
            None,
            cx,
        );

        let draft = state.selected_session().unwrap();
        assert_eq!(draft.meta.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            draft
                .meta
                .option_selections
                .iter()
                .find(|selection| selection.id == "reasoningEffort")
                .map(|selection| &selection.value),
            Some(&serde_json::json!("max"))
        );

        state.host.set_active_model(
            state.selected.as_deref().unwrap_or_default(),
            ProviderKind::Codex,
            Some("gpt-5.6-sol".into()),
            Some("second-account".into()),
            cx,
        );

        let draft = state.selected_session().unwrap();
        assert_eq!(
            draft
                .meta
                .option_selections
                .iter()
                .find(|selection| selection.id == "reasoningEffort")
                .map(|selection| &selection.value),
            Some(&serde_json::json!("low"))
        );
    });
}

#[test]
fn reopened_command_cache_seeds_a_draft_before_provider_start() {
    let test_store = TestStore::new("tcode-command-seed-test");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    let commands = vec![ProviderCommand {
        name: "review".into(),
        description: Some("Review changes".into()),
        kind: agent::ProviderCommandKind::Command,
    }];
    store
        .save_commands(
            &CommandsCacheKey::Native {
                provider: ProviderKind::ClaudeCode,
                home: None,
            },
            &commands,
        )
        .unwrap();

    let state = TestClientState::new(SessionStore::open_at(root.clone()).unwrap());
    let seeded = state.cached_provider_commands(ProviderKind::ClaudeCode, None, None);
    let draft = AppState::build_draft_session(
        "project".into(),
        PathBuf::from("/tmp/project"),
        ProviderKind::ClaudeCode,
        None,
        None,
        seeded,
    );
    assert_eq!(draft.provider_commands, commands);
    assert!(matches!(draft.runtime, Runtime::Idle));
}

/// Settings → Harnesses env/home/launch-args must reach the spawn options,
/// and the home override must land on the provider's own variable.
#[test]
fn provider_binary_home_and_launch_arguments_follow_the_selected_profile() {
    let mut settings = Settings::default();
    let claude = settings.provider_mut(ProviderKind::ClaudeCode);
    claude.binary_path = Some(PathBuf::from("/custom/claude"));
    claude.home_path = Some(PathBuf::from("/tmp/claude-home"));
    claude.launch_args = Some("--chrome --verbose".into());
    let codex = settings.provider_mut(ProviderKind::Codex);
    codex.binary_path = Some(PathBuf::from("/custom/codex"));
    codex.home_path = Some(PathBuf::from("/tmp/codex-shadow"));
    let pi = settings.provider_mut(ProviderKind::Pi);
    pi.launch_args = Some("--verbose".into());

    let launch_env = LaunchEnv {
        env: vec![("ANTHROPIC_BASE_URL".into(), "https://proxy.test".into())],
        home: settings
            .provider(ProviderKind::ClaudeCode)
            .home_path
            .clone(),
    };
    let meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
    let opts = session_options(&meta, &settings, launch_env, McpServers::default());
    assert_eq!(opts.binary_path, Some(PathBuf::from("/custom/claude")));
    assert_eq!(opts.extra_args, vec!["--chrome", "--verbose"]);
    assert_eq!(
        opts.launch_env.pairs(ProviderKind::ClaudeCode),
        vec![
            (
                "ANTHROPIC_BASE_URL".to_string(),
                "https://proxy.test".to_string()
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                "/tmp/claude-home".to_string()
            ),
        ]
    );

    // Codex takes its home as CODEX_HOME; this profile has no launch arguments.
    let launch_env = LaunchEnv {
        env: Vec::new(),
        home: settings.provider(ProviderKind::Codex).home_path.clone(),
    };
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
    let opts = session_options(&meta, &settings, launch_env, McpServers::default());
    assert_eq!(opts.binary_path, Some(PathBuf::from("/custom/codex")));
    assert!(opts.extra_args.is_empty());
    assert_eq!(
        opts.launch_env.pairs(ProviderKind::Codex),
        vec![("CODEX_HOME".to_string(), "/tmp/codex-shadow".to_string())]
    );

    // pi project trust is opt-in and appends --approve after launch args.
    let meta = SessionMeta::new(ProviderKind::Pi, PathBuf::from("/x"), None);
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        McpServers::default(),
    );
    assert_eq!(opts.extra_args, vec!["--verbose"]);

    settings
        .provider_mut(ProviderKind::Pi)
        .pi
        .trust_project_extensions = true;
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        McpServers::default(),
    );
    assert_eq!(opts.extra_args, vec!["--verbose", "--approve"]);
}

/// Sensitive env rows contribute their value from `secrets.json`, never from
/// settings.json (which stores an empty value for them).
/// A third-party Claude profile ("Klaude Kode" → Kimi) launches against its
/// own endpoint, binary, and key, in parallel with the untouched official
/// Claude profile. This is the end-to-end proof of profile-ization at the
/// launch layer.
#[test]
fn third_party_profile_launches_in_parallel_with_builtin() {
    let test_store = TestStore::new("tcode-profile-env");
    let store = (*test_store).clone();
    let mut state = TestClientState::new(store);

    let mut settings = state.settings.clone();
    // Official Claude keeps its own key.
    settings.provider_mut(ProviderKind::ClaudeCode).env = vec![
        EnvVar {
            name: "PLAIN".into(),
            value: "visible".into(),
            sensitive: false,
        },
        EnvVar {
            name: "ANTHROPIC_API_KEY".into(),
            value: String::new(),
            sensitive: true,
        },
        EnvVar {
            name: "UNSET_SECRET".into(),
            value: String::new(),
            sensitive: true,
        },
    ];
    // Every account of the harness gets its shared variables first; an
    // account's own value of the same name wins.
    settings.provider_mut(ProviderKind::ClaudeCode).shared_env = vec![
        EnvVar {
            name: "SHARED".into(),
            value: "all".into(),
            sensitive: false,
        },
        EnvVar {
            name: "SHARED_KEY".into(),
            value: String::new(),
            sensitive: true,
        },
        EnvVar {
            name: "ANTHROPIC_MODEL".into(),
            value: "shared-model".into(),
            sensitive: false,
        },
    ];
    // A user "Klaude Kode" profile pointing at Kimi's Anthropic-compatible
    // endpoint, with its own binary and (sensitive) key.
    settings.profiles.insert(
        "klaude-kode".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                display_name: Some("Klaude Kode".into()),
                env: vec![
                    EnvVar {
                        name: "ANTHROPIC_BASE_URL".into(),
                        value: "https://api.kimi.com/coding/".into(),
                        sensitive: false,
                    },
                    EnvVar {
                        name: "ANTHROPIC_MODEL".into(),
                        value: "k3[1m]".into(),
                        sensitive: false,
                    },
                    EnvVar {
                        name: "ANTHROPIC_API_KEY".into(),
                        value: String::new(),
                        sensitive: true,
                    },
                ],
                binary_path: Some(PathBuf::from("/opt/kimi/claude")),
                ..ProviderSettings::default()
            },
        },
    );
    state.settings = settings;
    state
        .settings_store
        .set_profile_secret(
            Settings::builtin_profile_id(ProviderKind::ClaudeCode),
            "ANTHROPIC_API_KEY",
            Some("sk-official"),
        )
        .unwrap();
    state
        .settings_store
        .set_profile_secret("klaude-kode", "ANTHROPIC_API_KEY", Some("sk-kimi"))
        .unwrap();
    let harness = Settings::harness_secret_id(ProviderKind::ClaudeCode);
    state
        .settings_store
        .set_profile_secret(&harness, "SHARED_KEY", Some("sk-shared"))
        .unwrap();
    let shared = [
        ("SHARED".to_string(), "all".to_string()),
        ("SHARED_KEY".to_string(), "sk-shared".to_string()),
        ("ANTHROPIC_MODEL".to_string(), "shared-model".to_string()),
    ];

    // The profile's launch env carries the shared variables, then the Kimi
    // endpoint + its own key, whose model wins over the shared one.
    let env = launch_env_for_profile(
        &state.settings,
        "klaude-kode",
        state.settings_store.profile_secrets(&harness),
        state.settings_store.profile_secrets("klaude-kode"),
    )
    .env;
    assert_eq!(env[..3], shared[..]);
    assert!(env.contains(&(
        "ANTHROPIC_BASE_URL".to_string(),
        "https://api.kimi.com/coding/".to_string()
    )));
    // The child takes the last value a name is given.
    assert_eq!(
        LaunchEnv {
            env: env.clone(),
            home: None
        }
        .child_path(ProviderKind::ClaudeCode, "ANTHROPIC_MODEL"),
        Some(PathBuf::from("k3[1m]"))
    );
    assert!(env.contains(&("ANTHROPIC_API_KEY".to_string(), "sk-kimi".to_string())));

    // The built-in profile is untouched: official key, no third-party URL.
    let builtin_profile_id = Settings::builtin_profile_id(ProviderKind::ClaudeCode);
    let builtin = launch_env_for_profile(
        &state.settings,
        builtin_profile_id,
        state.settings_store.profile_secrets(&harness),
        state.settings_store.profile_secrets(builtin_profile_id),
    )
    .env;
    assert_eq!(
        builtin,
        shared
            .iter()
            .cloned()
            .chain([
                ("PLAIN".to_string(), "visible".to_string()),
                ("ANTHROPIC_API_KEY".to_string(), "sk-official".to_string()),
            ])
            .collect::<Vec<_>>()
    );
    let cx = TestAppContext::default().host_cx();
    let builtin_meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
    assert_eq!(
        smol::block_on(state.prepare_session_env(&builtin_meta, &cx))
            .unwrap()
            .env,
        builtin
    );
    assert!(!builtin.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));

    // A session bound to the profile resolves the profile's env + binary,
    // while its protocol stays ClaudeCode.
    let mut meta = SessionMeta::new(
        ProviderKind::ClaudeCode,
        PathBuf::from("/x"),
        Some("k3[1m]".into()),
    );
    meta.profile_id = Some("klaude-kode".into());
    assert_eq!(
        state.resolve_profile_binary("klaude-kode"),
        Some(PathBuf::from("/opt/kimi/claude"))
    );
    let launch_env = smol::block_on(state.prepare_session_env(&meta, &cx)).unwrap();
    assert_eq!(launch_env.env, env);
    assert!(!launch_env.env.iter().any(|(key, _)| key == "UNSET_SECRET"));
    assert!(
        launch_env
            .env
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://api.kimi.com/coding/")
    );
    let opts = session_options(&meta, &state.settings, launch_env, McpServers::default());
    assert_eq!(opts.binary_path, Some(PathBuf::from("/opt/kimi/claude")));
}

#[test]
fn session_launch_scopes_mcp_registrations() {
    let registration = |name: &str, port| agent::McpRegistration {
        name: name.into(),
        url: format!("http://127.0.0.1:{port}/mcp"),
        bearer_token: format!("{name}-token"),
    };
    let preview = registration(agent::McpRegistration::SERVER_NAME_PREVIEW, 7);
    let computer = registration(agent::McpRegistration::SERVER_NAME_COMPUTER_USE, 10);
    let control = registration(agent::McpRegistration::SERVER_NAME_CONTROL, 11);
    for (provider, preview_supported, computer_supported) in [
        (ProviderKind::ClaudeCode, true, true),
        (ProviderKind::Codex, true, false),
        (ProviderKind::Pi, false, true),
    ] {
        let mut settings = Settings::default();
        for computer_enabled in [false, true] {
            settings.computer_use.enabled = computer_enabled;
            let meta = SessionMeta::new(provider, PathBuf::from("/x"), None);
            let options = session_options(
                &meta,
                &settings,
                LaunchEnv::default(),
                McpServers {
                    preview: Some(preview.clone()),
                    computer_use: Some(computer.clone()),
                    control: Some(control.clone()),
                },
            );
            let mut expected = Vec::new();
            for (enabled, reg) in [
                (preview_supported, &preview),
                (computer_enabled && computer_supported, &computer),
                (preview_supported, &control),
            ] {
                if enabled {
                    expected.push((&reg.name, &reg.url, &reg.bearer_token));
                }
            }
            assert_eq!(
                options
                    .mcp_servers
                    .iter()
                    .map(|reg| (&reg.name, &reg.url, &reg.bearer_token))
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
}

#[test]
fn report_callbacks_carry_the_report_verbatim_and_label_fallbacks() {
    use tcode_core::notice::{
        CallbackEvent, HostNotice, ORCHESTRATE_FALLBACK_LABEL, ORCHESTRATE_REPORT_LABEL,
        OrchestrateCallback, host_notice,
    };
    fn only_callback(text: &str) -> OrchestrateCallback {
        let Some(HostNotice::Orchestrate(mut callbacks)) = host_notice(text) else {
            panic!("not a callback: {text}");
        };
        assert_eq!(callbacks.len(), 1);
        callbacks.remove(0)
    }

    // A report is the child's own words: never truncated, never supplemented.
    let report = "界".repeat(5000);
    let text = assemble_report_callback(
        "child",
        "Title",
        "reported",
        CallbackBody::Report(&report),
        None,
    );
    assert_eq!(
        text,
        format!(
            "[orchestrate] thread child (\"Title\") reported.\n{ORCHESTRATE_REPORT_LABEL}{report}"
        )
    );
    let callback = only_callback(&text);
    assert_eq!(callback.event, CallbackEvent::Ended("reported".into()));
    assert_eq!(
        callback.report(),
        Some(report.as_str()),
        "the lead's view shows the report alone"
    );

    // A fallback is labelled as such and bounded.
    for (output, expected_tail, truncated) in [
        (String::new(), "(no assistant output)".into(), false),
        ("done".into(), "done".into(), false),
        ("界".repeat(1200), "界".repeat(1200), false),
        ("界".repeat(1201), "界".repeat(600), true),
    ] {
        let text = assemble_report_callback(
            "child",
            "Title",
            "unreported",
            CallbackBody::Fallback(&output),
            None,
        );
        assert!(text.starts_with(&format!(
            "[orchestrate] thread child (\"Title\") unreported.\n{ORCHESTRATE_FALLBACK_LABEL}"
        )));
        assert!(!text.contains("tokens:"));
        assert_eq!(text.contains("Final output tail"), truncated);
        assert_eq!(text.lines().last().unwrap(), expected_tail);
        let callback = only_callback(&text);
        assert_eq!(callback.event, CallbackEvent::Ended("unreported".into()));
        assert_eq!(
            callback.report(),
            Some(expected_tail.as_str()),
            "the lead's view shows the output"
        );
    }

    let usage = agent::TokenUsage {
        input_tokens: Some(100),
        cached_input_tokens: Some(25),
        output_tokens: Some(40),
        total_processed_tokens: Some(165),
        ..Default::default()
    };
    assert_eq!(
        assemble_report_callback(
            "child",
            "Title",
            "failed",
            CallbackBody::Failure("the provider closed"),
            Some(&usage),
        ),
        "[orchestrate] thread child (\"Title\") failed. tokens: input 100 (+25 cached), output 40, total 165.\nthe provider closed"
    );
}

#[test]
fn only_report_result_reaches_the_lead_and_a_silent_child_is_reminded_then_fallen_back() {
    use tcode_core::notice::{ORCHESTRATE_FALLBACK_LABEL, ORCHESTRATE_REPORT_LABEL};
    use tcode_core::settlement::AgentReport;

    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-report-result-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();
    let (silent_commands, silent_receiver) = smol::channel::unbounded();
    let callbacks = || {
        std::iter::from_fn(|| parent_receiver.try_recv().ok())
            .filter_map(|command| match command {
                SessionCommand::Steer { text, .. } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
    };

    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state.sessions.push(parent.meta.clone());
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        for (id, commands) in [
            ("reporter", smol::channel::unbounded().0),
            ("silent", silent_commands),
        ] {
            let mut child = live_session(ProviderKind::Codex, commands);
            child.meta.id = id.into();
            child.meta.parent_session_id = Some("parent".into());
            child.meta.expectation = Some(ChildExpectation::opened(1));
            child.turn_in_flight = true;
            state.sessions.push(child.meta.clone());
            state.residents.parked.insert(child.meta.id.clone(), child);
        }

        // A report from a thread no other thread started as its subagent is refused.
        let report = |text: &str| {
            control_mcp::ControlOp::ReportResult(control_mcp::ReportResultParams {
                text: text.into(),
            })
        };
        assert!(control_call(state, cx, "parent", report("nope")).is_err());
        assert_eq!(
            control_call(state, cx, "reporter", report("the full reported RESULT")).unwrap()
                ["delivered_when"],
            "turn_ends"
        );

        // The report rides in the child's own transcript as the tool call.
        state.on_event("reporter", persisted_assistant_event("working"), cx);
        state.on_event(
            "reporter",
            report_result_event("rep-1", "the full reported RESULT"),
            cx,
        );
        state.on_event("reporter", persisted_assistant_event("done"), cx);
        state.on_event("reporter", completed_turn("turn-1"), cx);
        state.on_event("silent", persisted_assistant_event("last message"), cx);
        state.on_event("silent", completed_turn("turn-1"), cx);
    });
    cx.run_until_parked();

    let delivered = callbacks();
    assert_eq!(
        delivered.len(),
        1,
        "only the report is delivered: {delivered:?}"
    );
    assert!(delivered[0].starts_with("[orchestrate] thread reporter (\""));
    assert!(delivered[0].contains(&format!(
        "reported.\n{ORCHESTRATE_REPORT_LABEL}the full reported RESULT"
    )));
    assert!(!delivered[0].contains("done"));
    state.read(|state| {
        assert_eq!(
            state.find_meta("reporter").unwrap().report(),
            AgentReport::Reported
        );
        let silent = state.find_meta("silent").unwrap();
        assert_eq!(silent.report(), AgentReport::Expected);
        assert!(silent.expectation.as_ref().unwrap().reminded);
    });
    // The silent child is reminded once, visibly, instead of being reported on.
    let reminder = match silent_receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) => {
            assert_eq!(text, CHILD_REPORT_REMINDER);
            delivery_id
        }
        other => panic!("expected the reminder as the silent child's next turn, got {other:?}"),
    };

    state.update(cx, |state, cx| {
        // The reporter wakes itself (a monitor, a scheduled wake-up): nothing
        // new for the lead.
        state.on_event(
            "reporter",
            AgentEvent::TurnStarted {
                turn_id: "turn-2".into(),
            },
            cx,
        );
        state.on_event(
            "reporter",
            persisted_assistant_event("Four of 10 done."),
            cx,
        );
        state.on_event("reporter", completed_turn("turn-2"), cx);

        // The silent child runs the reminder turn and still does not report.
        state.on_event(
            "silent",
            AgentEvent::TurnAccepted {
                delivery_id: reminder,
            },
            cx,
        );
        state.on_event(
            "silent",
            AgentEvent::TurnStarted {
                turn_id: "turn-2".into(),
            },
            cx,
        );
        state.on_event("silent", persisted_assistant_event("still nothing"), cx);
        state.on_event("silent", completed_turn("turn-2"), cx);
    });
    cx.run_until_parked();

    let delivered = callbacks();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert!(delivered[0].starts_with("[orchestrate] thread silent (\""));
    assert!(delivered[0].contains(&format!(
        "unreported.\n{ORCHESTRATE_FALLBACK_LABEL}still nothing"
    )));
    state.read(|state| {
        assert_eq!(
            state.find_meta("silent").unwrap().report(),
            AgentReport::Unreported
        );
    });
    assert!(silent_receiver.try_recv().is_err(), "no second reminder");

    // A late report still reaches the lead, once.
    state.update(cx, |state, cx| {
        state.on_event(
            "silent",
            AgentEvent::TurnStarted {
                turn_id: "turn-3".into(),
            },
            cx,
        );
        state.on_event("silent", report_result_event("rep-2", "late report"), cx);
        state.on_event("silent", completed_turn("turn-3"), cx);
        state.on_event(
            "silent",
            AgentEvent::TurnStarted {
                turn_id: "turn-4".into(),
            },
            cx,
        );
        state.on_event("silent", completed_turn("turn-4"), cx);
    });
    cx.run_until_parked();
    let delivered = callbacks();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert!(delivered[0].contains(&format!("reported.\n{ORCHESTRATE_REPORT_LABEL}late report")));
}

/// One `tcode_control` call by `caller`, answered on the host thread.
fn control_call(
    state: &mut AppState,
    cx: &mut HostCx,
    caller: &str,
    op: control_mcp::ControlOp,
) -> Result<serde_json::Value, String> {
    let (reply, response) = smol::channel::bounded(1);
    state.handle_control_op(caller.into(), op, reply, cx);
    response.try_recv().unwrap()
}

/// The child's `report_result` call as its transcript records it.
fn report_result_event(id: &str, text: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: ItemContent::ToolCall {
            image_reads: Vec::new(),
            name: "mcp__tcode_control__report_result".into(),
            input: serde_json::json!({ "text": text }),
            output: Some("{\"ok\":true}".into()),
            status: ItemStatus::Completed,
        },
    })
}

fn completed_turn(turn_id: &str) -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: turn_id.into(),
        status: TurnStatus::Completed,
        usage: None,
    }
}

/// Recorded from a dispatched Claude Code child (thread 660c1acb…, October
/// 2026): its brief ran a background script, the Monitor tool woke it nine
/// times with one-line progress messages (turns 2–10), and it reported once,
/// in turn 12. Its lead received twelve "completed" callbacks. The contract is
/// one: the report. `BackgroundTasksChanged` is not persisted, so the Monitor's
/// liveness is replayed the way the CLI publishes it — set when the monitor
/// starts, cleared with the result of the turn in which it ended.
#[test]
fn a_child_waking_itself_reports_to_its_lead_once() {
    use tcode_core::session::StoredEvent;
    use tcode_core::settlement::AgentReport;

    const RECORDING: &str = include_str!("fixtures/monitor_wakeups_child.jsonl");

    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-monitor-wakeups");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();
    let (child_commands, child_receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::ClaudeCode, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state.sessions.push(parent.meta.clone());
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);
        let mut child = live_session(ProviderKind::ClaudeCode, child_commands);
        child.meta.id = "child".into();
        child.meta.title = "测试注解改成人话（10 条样例）".into();
        child.meta.parent_session_id = Some("parent".into());
        child.meta.expectation = Some(ChildExpectation::opened(1));
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        let records: Vec<StoredEvent> = RECORDING
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let last_turn = records
            .iter()
            .rev()
            .find_map(|record| match &record.event {
                AgentEvent::TurnCompleted { turn_id, .. } => Some(turn_id.clone()),
                _ => None,
            })
            .unwrap();
        for record in records {
            let monitor_started = matches!(
                &record.event,
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::ToolCall { name, .. },
                    ..
                }) if name == "Monitor"
            );
            if matches!(&record.event, AgentEvent::TurnCompleted { turn_id, .. } if *turn_id == last_turn)
            {
                state.on_event("child", AgentEvent::BackgroundTasksChanged { count: 0 }, cx);
            }
            if let AgentEvent::TurnStarted { .. } = &record.event {
                state.resident_mut("child").unwrap().turn_in_flight = true;
            }
            state.on_event("child", record.event, cx);
            if monitor_started {
                state.on_event("child", AgentEvent::BackgroundTasksChanged { count: 1 }, cx);
            }
        }
    });
    cx.run_until_parked();

    let callbacks: Vec<_> = std::iter::from_fn(|| parent_receiver.try_recv().ok())
        .filter_map(|command| match command {
            SessionCommand::Steer { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        callbacks.len(),
        1,
        "one report, one callback: {callbacks:?}"
    );
    assert!(
        callbacks[0].starts_with(
            "[orchestrate] thread child (\"测试注解改成人话（10 条样例）\") reported."
        )
    );
    assert!(callbacks[0].ends_with("\n<report>"));
    assert!(
        !callbacks.iter().any(|text| text.contains("of 10 done")),
        "progress lines stay in the child"
    );
    assert!(
        std::iter::from_fn(|| child_receiver.try_recv().ok())
            .all(|command| !matches!(command, SessionCommand::SendTurn { .. })),
        "a child waiting on its monitor is not reminded"
    );
    state.read(|state| {
        assert_eq!(
            state.find_meta("child").unwrap().report(),
            AgentReport::Reported
        );
    });
}

/// A follow-up to a subagent whose log is not loaded yet is owed a new report: the report
/// the subagent made for the previous message is not delivered again as its answer.
#[test]
fn a_follow_up_to_an_unloaded_subagent_is_not_answered_by_its_earlier_report() {
    use tcode_core::settlement::AgentReport;

    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-subagent-follow-up-unloaded");
    let mut child = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
    child.id = "child".into();
    child.parent_session_id = Some("parent".into());
    child.expectation = Some(ChildExpectation {
        state: AgentReport::Reported,
        delivered_item: Some("rep-old".into()),
        ..ChildExpectation::opened(1)
    });
    store.upsert_meta(&child).unwrap();
    let earlier = now_millis() - 60_000;
    for event in [
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        report_result_event("rep-old", "the earlier report"),
        completed_turn("turn-1"),
    ] {
        store.append_event("child", earlier, &event).unwrap();
    }
    let scripted = scripted_provider(ProviderKind::Codex);
    let state = cx.new_entity({
        let mut state = TestClientState::new((*store).clone());
        state.set_provider_launcher_for_test(scripted.launcher);
        state
    });
    let (parent_commands, parent_receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state.sessions.push(parent.meta.clone());
        state.residents.parked.insert("parent".into(), parent);
        assert!(state.resident("child").is_none());
        let sent = control_call(
            state,
            cx,
            "parent",
            control_mcp::ControlOp::SendMessage(control_mcp::SendMessageParams {
                thread_ids: vec!["child".into()],
                text: "One more check".into(),
                mode: control_mcp::SendMode::Queue,
                attachments: None,
                request_id: None,
            }),
        )
        .unwrap();
        assert_eq!(sent["results"][0]["outcome"], "applied", "{sent}");
    });
    cx.run_until_parked();
    state.update(cx, |state, cx| {
        state.on_event(
            "child",
            AgentEvent::TurnStarted {
                turn_id: "turn-2".into(),
            },
            cx,
        );
        state.on_event("child", persisted_assistant_event("checked, no report"), cx);
        state.on_event("child", completed_turn("turn-2"), cx);
    });
    cx.run_until_parked();

    let callbacks: Vec<_> = std::iter::from_fn(|| parent_receiver.try_recv().ok())
        .filter_map(|command| match command {
            SessionCommand::Steer { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(callbacks.is_empty(), "{callbacks:?}");
    state.read(|state| {
        let child = state.find_meta("child").unwrap();
        assert_eq!(child.report(), AgentReport::Expected);
    });
}

/// An orchestrator returning to a long-idle child whose worktree has since been
/// removed must hear that the child could not start and why, not nothing (the
/// failure folds into the already-reported turn) or that turn's old output.
#[test]
fn send_to_child_whose_cwd_was_removed_reports_the_start_failure() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-send-missing-cwd-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();
    let (child_commands, _child_receiver) = smol::channel::unbounded();
    let cwd = std::env::temp_dir().join(format!("tcode-removed-{}", uuid::Uuid::new_v4()));

    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.meta.project_id = Some("project".into());
        parent.turn_in_flight = true;
        state.sessions.push(parent.meta.clone());
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        let mut child = live_session(ProviderKind::Codex, child_commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        child.meta.cwd = cwd.clone();
        child.meta.project_id = Some("project".into());
        child.meta.expectation = Some(ChildExpectation::opened(1));
        child.turn_in_flight = true;
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        state.on_event("child", report_result_event("rep-1", "old report"), cx);
        state.on_event("child", completed_turn("turn-1"), cx);
    });
    cx.run_until(|_| !parent_receiver.is_empty());
    assert!(matches!(
        parent_receiver.try_recv(),
        Ok(SessionCommand::Steer { text, .. }) if text.ends_with("\nold report")
    ));

    state.update(cx, |state, cx| {
        state.resident_mut("child").unwrap().shutdown_to_idle();
        let sent = control_call(
            state,
            cx,
            "parent",
            control_mcp::ControlOp::SendMessage(control_mcp::SendMessageParams {
                thread_ids: vec!["child".into()],
                text: "continue".into(),
                mode: control_mcp::SendMode::Queue,
                attachments: None,
                request_id: None,
            }),
        )
        .unwrap();
        assert_eq!(sent["results"][0]["outcome"], "applied", "{sent}");
    });
    cx.run_until(|_| !parent_receiver.is_empty());
    assert!(
        cx.drain_outgoing().iter().any(|message| matches!(
            message,
            HostMessage::Event(EventEnvelope {
                event: ServerEvent::Runtime(RuntimeEvent::Error(
                    RuntimeError::WorkingDirectoryMissing { session_id, project_id: Some(project_id), cwd: missing }
                )),
                ..
            }) if session_id == "child" && project_id == "project" && missing == &cwd
        )),
        "a missing working directory is reported with its project so a client can offer to re-point it"
    );

    let Ok(SessionCommand::Steer { text, .. }) = parent_receiver.try_recv() else {
        panic!("the parent must be told the child failed to start");
    };
    assert_eq!(
        text,
        format!(
            "[orchestrate] thread child (\"{}\") failed.\nfailed to spawn provider process: working directory `{}` no longer exists",
            state.read(|state| state.find_meta("child").unwrap().title.clone()),
            cwd.display()
        )
    );

    state.update(cx, |state, cx| {
        let thread = control_call(
            state,
            cx,
            "parent",
            control_mcp::ControlOp::GetThread(control_mcp::GetThreadParams {
                thread_id: "child".into(),
            }),
        )
        .unwrap();
        assert_eq!(thread["report"], "failed", "{thread}");
    });
}

#[test]
fn child_native_approval_callback_and_respond_approval_preserve_options_and_scope() {
    use agent::ApprovalOptionKind::{AllowAlways, AllowOnce, RejectOnce};
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-native-child-approvals");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();
    let (child_commands, child_receiver) = smol::channel::unbounded();
    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        let mut child = live_session(ProviderKind::Codex, child_commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        let mut other = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp"), None);
        other.id = "other-parent".into();
        state.sessions.extend([parent.meta.clone(), child.meta.clone(), other]);
        state.residents.parked.insert("parent".into(), parent);
        state.residents.parked.insert("child".into(), child);
        let respond = |state: &mut AppState, cx: &mut HostCx, caller: &str, approval: Option<&str>, option: Option<&str>, cancel: bool| {
            control_call(state, cx, caller, control_mcp::ControlOp::RespondApproval(control_mcp::RespondApprovalParams {
                thread_id: "child".into(),
                approval_id: approval.map(str::to_string),
                option: option.map(str::to_string),
                cancel,
            }))
        };
        for (n, mode) in [ChildApprovalMode::Orchestrator, ChildApprovalMode::Auto, ChildApprovalMode::Manual, ChildApprovalMode::AlwaysAllow].into_iter().enumerate() {
            state.settings.subagents.child_approval = mode;
            let request_id = format!("request-{n}");
            let options = if mode == ChildApprovalMode::AlwaysAllow {
                vec![agent::ApprovalOption { id: "Reject:Native".into(), label: "Reject".into(), kind: RejectOnce }]
            } else {
                vec![
                    agent::ApprovalOption { id: "Allow:Native".into(), label: "Native allowance".into(), kind: AllowOnce },
                    agent::ApprovalOption { id: "Reject:Native".into(), label: "Reject".into(), kind: RejectOnce },
                ]
            };
            state.on_event("child", AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                parent_item_id: None,
                id: request_id.clone(), turn_id: None,
                kind: agent::ApprovalKind::ToolUse { name: "mcp__tcode_control__report_result".into(), input: serde_json::json!({}), detail: "Native MCP consent".into() },
                reason: None,
                blocked_path: None,
                options: options.clone(),
            }), cx);
            assert!(child_receiver.try_recv().is_err(), "requests without automatic allow must remain pending");
            let SessionCommand::Steer { text, .. } = parent_receiver.try_recv().unwrap() else { panic!("approval must reach lead") };
            assert!(text.contains(&request_id));
            let native = text.split_once("Native options: ").unwrap().1.split_once(". ").unwrap().0;
            let offered: Vec<agent::ApprovalOption> = serde_json::from_str(native).unwrap();
            assert_eq!(offered, options);
            let manual = mode == ChildApprovalMode::Manual;
            if manual {
                assert!(text.contains("The user answers in the child thread."));
            } else {
                assert!(text.contains("respond_approval"));
                assert!(text.contains("option: <exact id> or cancel: true"));
            }
            let refusals = [
                ("other-parent", None, Some("Reject:Native"), false, "not your subagent"),
                ("parent", Some("missing"), Some("Reject:Native"), false, "is not pending"),
                ("parent", None, Some("invented-id"), false, "valid option ids"),
                ("parent", None, Some("Reject:Native"), true, "never both"),
                ("parent", None, None, false, "never both"),
            ];
            // In manual mode the approval is the user's, whatever the lead asks.
            let manual_refusal = [("parent", None, Some("Reject:Native"), false, "subagent approvals is manual")];
            for (caller, approval, option, cancel, error) in if manual { &manual_refusal[..] } else { &refusals[..] } {
                let refused = respond(state, cx, caller, *approval, *option, *cancel).unwrap_err();
                assert!(refused.contains(error), "{refused}");
                assert!(child_receiver.try_recv().is_err());
                assert_eq!(state.approval_requests("child").len(), 1);
            }
            let chosen = offered.first().unwrap().id.clone();
            if manual {
                state.respond_session_approval("child", request_id.clone(), ApprovalDecision::Cancel).unwrap();
            } else {
                assert_eq!(respond(state, cx, "parent", None, Some(&chosen), false).unwrap()["approval_id"], request_id);
            }
            let SessionCommand::RespondApproval { request_id: delivered, decision } = child_receiver.try_recv().unwrap() else { panic!("missing native approval response") };
            assert_eq!(delivered, request_id);
            assert_eq!(decision, if manual { ApprovalDecision::Cancel } else { ApprovalDecision::Option(chosen) });
            assert!(state.approval_requests("child").is_empty());
        }
        state.settings.subagents.child_approval = ChildApprovalMode::AlwaysAllow;
        for (n, kinds, expected) in [
            (0, vec![RejectOnce, AllowOnce, AllowAlways, AllowOnce], "native-2"),
            (1, vec![RejectOnce, AllowOnce], "native-1"),
        ] {
            let request_id = format!("automatic-{n}");
            state.on_event("child", AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                parent_item_id: None,
                id: request_id.clone(), turn_id: None,
                kind: agent::ApprovalKind::ExecCommand { command: "cargo check".into(), cwd: None, description: None },
                reason: None,
                blocked_path: None,
                options: kinds.into_iter().enumerate().map(|(i, kind)| agent::ApprovalOption { id: format!("native-{i}"), label: "Native choice".into(), kind }).collect(),
            }), cx);
            assert!(matches!(child_receiver.try_recv(), Ok(SessionCommand::RespondApproval { request_id: delivered, decision: ApprovalDecision::Option(option) }) if delivered == request_id && option == expected));
            assert!(parent_receiver.try_recv().is_err());
            assert!(state.approval_requests("child").is_empty());
        }
    });
}

#[test]
fn child_questions_reach_the_lead_and_respond_input_answers_them() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-child-questions");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();
    let (child_commands, child_receiver) = smol::channel::unbounded();
    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        let mut child = live_session(ProviderKind::Codex, child_commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        let mut other = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp"), None);
        other.id = "other-parent".into();
        state
            .sessions
            .extend([parent.meta.clone(), child.meta.clone(), other]);
        let respond = |state: &mut AppState,
                       cx: &mut HostCx,
                       caller: &str,
                       question: Option<&str>,
                       answers: serde_json::Map<String, serde_json::Value>| {
            control_call(
                state,
                cx,
                caller,
                control_mcp::ControlOp::RespondInput(control_mcp::RespondInputParams {
                    thread_id: "child".into(),
                    question_id: question.map(str::to_string),
                    answers,
                    request_id: None,
                }),
            )
        };
        state.residents.parked.insert("parent".into(), parent);
        state.residents.parked.insert("child".into(), child);
        let questions = vec![
            agent::UserInputQuestion {
                id: "lang".into(),
                header: "Language".into(),
                question: "Which language?".into(),
                options: vec![agent::UserInputOption {
                    label: "Rust".into(),
                    description: "Native".into(),
                }],
                multi_select: false,
                prefill: None,
            },
            agent::UserInputQuestion {
                id: "targets".into(),
                header: "Targets".into(),
                question: "Which targets?".into(),
                options: vec![
                    agent::UserInputOption {
                        label: "ios".into(),
                        description: String::new(),
                    },
                    agent::UserInputOption {
                        label: "android".into(),
                        description: String::new(),
                    },
                ],
                multi_select: true,
                prefill: None,
            },
        ];
        for (n, mode, delivery) in [
            (
                0,
                ChildApprovalMode::Auto,
                agent::UserInputDelivery::Blocking,
            ),
            (
                1,
                ChildApprovalMode::Manual,
                agent::UserInputDelivery::Async,
            ),
        ] {
            state.settings.subagents.child_approval = mode;
            let request_id = format!("question-{n}");
            state.on_event(
                "child",
                AgentEvent::UserInputRequested {
                    request_id: request_id.clone(),
                    questions: questions.clone(),
                    delivery,
                },
                cx,
            );
            assert!(
                child_receiver.try_recv().is_err(),
                "a question is never answered automatically"
            );
            let SessionCommand::Steer { text, .. } = parent_receiver.try_recv().unwrap() else {
                panic!("question must reach lead")
            };
            assert!(text.contains(&request_id), "{text}");
            let asked = text
                .split_once("Questions: ")
                .unwrap()
                .1
                .split_once(". ")
                .unwrap()
                .0;
            let asked: Vec<agent::UserInputQuestion> = serde_json::from_str(asked).unwrap();
            assert_eq!(asked, questions);
            assert!(
                text.contains(if delivery.is_blocking() {
                    "blocked until"
                } else {
                    "keeps working"
                }),
                "{text}"
            );
            assert!(
                text.contains(if mode == ChildApprovalMode::Manual {
                    "The user answers in the child thread."
                } else {
                    "respond_input"
                }),
                "{text}"
            );
            let answers = |value: serde_json::Value| value.as_object().cloned().unwrap();
            for (parent, request, answers, error) in [
                (
                    "other-parent",
                    None,
                    answers(serde_json::json!({"lang": "Rust", "targets": []})),
                    "needs category answer_input",
                ),
                (
                    "parent",
                    Some("missing"),
                    answers(serde_json::json!({"lang": "Rust", "targets": []})),
                    "request missing is not pending",
                ),
                (
                    "parent",
                    None,
                    answers(serde_json::json!({"lang": "Rust", "colour": "red"})),
                    "unknown question id",
                ),
                (
                    "parent",
                    None,
                    answers(serde_json::json!({"lang": "Rust"})),
                    "missing answers for question ids [\"targets\"]",
                ),
            ] {
                let refused = respond(state, cx, parent, request, answers).unwrap_err();
                assert!(refused.contains(error), "{refused}");
                assert!(child_receiver.try_recv().is_err());
            }
            let answers =
                answers(serde_json::json!({"lang": "Rust", "targets": ["ios", "android"]}));
            assert_eq!(
                respond(state, cx, "parent", Some(&request_id), answers.clone()).unwrap()
                    ["request_id"],
                request_id
            );
            let SessionCommand::RespondUserInput {
                request_id: delivered,
                answers: sent,
            } = child_receiver.try_recv().unwrap()
            else {
                panic!("missing answer")
            };
            assert_eq!(delivered, request_id);
            assert_eq!(sent, answers);
            state.on_event(
                "child",
                AgentEvent::UserInputResolved {
                    request_id,
                    answers,
                },
                cx,
            );
            assert!(
                state
                    .resident("child")
                    .unwrap()
                    .timeline
                    .pending_user_input
                    .is_none()
            );
            assert!(
                respond(state, cx, "parent", None, serde_json::Map::new())
                    .unwrap_err()
                    .contains("has no pending user-input request")
            );
        }
    });
}

#[test]
fn an_unreported_fallback_is_the_final_message_of_the_last_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-fallback-final-message-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let report = format!("Complete answer.\n{}", "full detail ".repeat(80));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state.sessions.push(parent.meta.clone());
        state.residents.parked.insert("parent".into(), parent);

        let (commands, _receiver) = smol::channel::unbounded();
        let mut child = live_session(ProviderKind::Codex, commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        // Already reminded once, so going quiet again delivers the fallback.
        child.meta.expectation = Some(ChildExpectation {
            reminded: true,
            ..ChildExpectation::opened(1)
        });
        child.turn_in_flight = true;

        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        for (id, content) in [
            (
                "preamble",
                ItemContent::AssistantMessage {
                    text: "Earlier tool preamble.".into(),
                    summarized: false,
                },
            ),
            (
                "reasoning",
                ItemContent::Reasoning {
                    text: "private reasoning".into(),
                },
            ),
            (
                "final-1",
                ItemContent::AssistantMessage {
                    text: "Complete ".into(),
                    summarized: false,
                },
            ),
            (
                "final-2",
                ItemContent::AssistantMessage {
                    text: format!("answer.\n{}", "full detail ".repeat(80)),
                    summarized: false,
                },
            ),
        ] {
            state.on_event(
                "child",
                AgentEvent::ItemCompleted(ThreadItem {
                    id: id.into(),
                    parent_item_id: None,
                    content,
                }),
                cx,
            );
        }
        state.on_event(
            "child",
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
    });
    cx.run_until_parked();
    let Ok(SessionCommand::Steer { text, .. }) = parent_receiver.try_recv() else {
        panic!("the lead must receive the fallback");
    };
    // The adjacent blocks of the final message, without the earlier preamble or reasoning.
    assert!(
        text.ends_with(&format!(
            "{}{report}",
            tcode_core::notice::ORCHESTRATE_FALLBACK_LABEL
        )),
        "{text}"
    );
}

#[test]
fn steering_parked_orchestrator_callback_uses_recorded_id() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-steer-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, receiver) = smol::channel::unbounded();
    let mut recorded_request_id = String::new();

    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        state.deliver_orchestrate_callback_to_parent(
            "parent",
            "[orchestrate] child-a completed.\nfull result".into(),
            cx,
        );

        let parent = state.residents.parked.get("parent").unwrap();
        assert!(parent.queue.is_empty(), "parallel result must not queue");
        assert!(parent.turn_in_flight);
        let command = receiver.try_recv().unwrap();
        let SessionCommand::Steer {
            request_id, text, ..
        } = command
        else {
            panic!("callback did not steer")
        };
        recorded_request_id = request_id;
        assert!(text.contains("full result"));
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let timeline = Timeline::fold_events(state.store.read_events("parent").unwrap());
        assert!(timeline.entries.iter().any(|entry| matches!(
            &entry.content,
            EntryContent::Steer {
                text,
                status: tcode_core::session::SteeringStatus::Pending,
                ..
            } if entry.id == recorded_request_id && text.contains("child-a completed")
        )));
    });
}

#[test]
fn steering_user_and_queue_paths_send_the_same_id_they_record() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-user-steer-id-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "active".into();
        active.turn_in_flight = true;
        active.timeline.apply_at(
            None,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "opening".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "start".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
        );
        let sentinel = active.push_queued("queued sentinel".into(), Vec::new());
        state.install_selected(active);

        cx.delivery_key = Some("steer-send".into());
        state.steer("active", "redirect".into(), Vec::new(), cx);
        let SessionCommand::Steer { request_id, .. } = receiver.try_recv().unwrap() else {
            panic!("user steer command missing")
        };
        assert_eq!(request_id, "local-steer-steer-send");
        let active = state.selected_session().unwrap();
        assert!(active.turn_in_flight);
        assert_eq!(active.queue.len(), 1);
        assert_eq!(active.queue[0].id, sentinel);
        assert_eq!(active.queue[0].text, "queued sentinel");
        assert!(active.timeline.entries.iter().any(|entry| matches!(
            &entry.content,
            EntryContent::Steer {
                text,
                status: tcode_core::session::SteeringStatus::Pending,
                ..
            } if entry.id == request_id && text == "redirect"
        )));

        state.drop_queued("active", sentinel, cx);
        let active = state.selected_session_mut().unwrap();
        let first = active.push_queued("first".into(), Vec::new());
        let queued_id = active.push_queued("queued redirect".into(), Vec::new());
        let third = active.push_queued("third".into(), Vec::new());
        active
            .queue
            .iter_mut()
            .find(|message| message.id == queued_id)
            .unwrap()
            .delivery_key = Some("queue-send".into());
        cx.delivery_key = Some("queue-steer-click".into());
        state.steer_queued("active", queued_id, cx);
        let SessionCommand::Steer { request_id, .. } = receiver.try_recv().unwrap() else {
            panic!("queue-to-steer command missing")
        };
        assert_eq!(request_id, "local-steer-queue-send");
        assert_eq!(cx.delivery_key.as_deref(), Some("queue-steer-click"));
        let active = state.selected_session().unwrap();
        assert_eq!(
            active
                .queue
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "third"]
        );
        assert!(active.timeline.entries.iter().any(|entry| matches!(
            &entry.content,
            EntryContent::Steer {
                text,
                status: tcode_core::session::SteeringStatus::Pending,
                ..
            } if entry.id == request_id && text == "queued redirect"
        )));
        state.drop_queued("active", first, cx);
        state.drop_queued("active", queued_id, cx);
        let active = state.selected_session_mut().unwrap();
        assert_eq!(active.queue.len(), 1);
        assert_eq!(active.queue[0].id, third);
        active.delivery_in_flight = Some(third);
        state.drop_queued("active", third, cx);
        state.steer_queued("active", third, cx);
        assert_eq!(state.selected_session().unwrap().queue[0].id, third);
        assert!(
            receiver.try_recv().is_err(),
            "an unacknowledged delivery cannot be steered twice"
        );
    });
}

/// The runtime routes a steer by its own turn flag, but the provider's
/// `turn/completed` can already be on the wire. The provider then rejects the
/// steer by id, and the text goes out as the next ordinary turn instead of
/// vanishing behind a notice.
#[test]
fn steer_rejected_after_turn_end_is_resent_as_a_new_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-steer-rejected-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "active".into();
        active.turn_in_flight = true;
        state.install_selected(active);

        cx.origin = Some(MessageOrigin::Agent);
        state.steer("active", "redirect".into(), Vec::new(), cx);
        cx.origin = None;
        let SessionCommand::Steer { request_id, .. } = next_command(&receiver).unwrap() else {
            panic!("steer command missing")
        };

        state.on_event(
            "active",
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(next_command(&receiver).is_err(), "nothing queued yet");

        state.on_event(
            "active",
            AgentEvent::SteerRejected {
                request_id: request_id.clone(),
            },
            cx,
        );
        match next_command(&receiver).unwrap() {
            SessionCommand::SendTurn { text, .. } => assert_eq!(text, "redirect"),
            other => panic!("expected the rejected steer to be sent as a turn, got {other:?}"),
        }
        let active = state.selected_session().unwrap();
        assert!(active.pending_steers.is_empty());
        assert!(
            !active
                .timeline
                .entries
                .iter()
                .any(|entry| entry.id == request_id),
            "the pending steer bubble is gone; the send records its own message"
        );
        assert_eq!(active.queue.len(), 1);
        assert_eq!(active.queue[0].origin, MessageOrigin::Agent);

        state.on_event(
            "active",
            AgentEvent::SteerRejected {
                request_id: request_id.clone(),
            },
            cx,
        );
        assert!(
            next_command(&receiver).is_err(),
            "a rejection for an already resolved steer sends nothing"
        );
    });
}

#[test]
fn callbacks_racing_provider_start_share_one_wakeup_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-start-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::ClaudeCode, smol::channel::unbounded().0);
        parent.meta.id = "parent".into();
        parent.runtime = Runtime::Starting { generation: 1 };
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        state.deliver_orchestrate_callback_to_parent(
            "parent",
            "[orchestrate] child-a completed.\nresult a".into(),
            cx,
        );
        state.deliver_orchestrate_callback_to_parent(
            "parent",
            "[orchestrate] child-b completed.\nresult b".into(),
            cx,
        );

        let parent = state.residents.parked.get("parent").unwrap();
        assert_eq!(parent.queue.len(), 1);
        assert_eq!(parent.queue[0].kind, QueuedMessageKind::OrchestrateCallback);
        assert!(parent.queue[0].text.contains("result a"));
        assert!(parent.queue[0].text.contains("result b"));

        let (commands, receiver) = smol::channel::unbounded();
        state.residents.parked.get_mut("parent").unwrap().runtime = Runtime::Live(commands);
        state.on_background_turn_completed("parent", cx);

        let delivery_id = match receiver.try_recv() {
            Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) if text.contains("result a") && text.contains("result b") => delivery_id,
            other => panic!("expected merged callback SendTurn, got {other:?}"),
        };
        assert_eq!(state.residents.parked["parent"].queue.len(), 1);
        state.on_event("parent", AgentEvent::TurnAccepted { delivery_id }, cx);
        let parent = state.residents.parked.get("parent").unwrap();
        assert!(parent.queue.is_empty());
        assert!(parent.turn_in_flight);
    });
}

#[test]
fn queued_sends_dispatch_one_per_completed_turn() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = ActiveSession {
        runtime: Runtime::Live(commands),
        ..ActiveSession::new(
            SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None),
            false,
            Vec::new(),
        )
    };
    active.push_queued("first".into(), Vec::new());
    active.push_queued("second".into(), Vec::new());

    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let first_delivery = match receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) if text == "first" => delivery_id,
        other => panic!("expected first SendTurn, got {other:?}"),
    };
    assert_eq!(active.dispatch_next_pending(), Ok(false));
    assert!(receiver.try_recv().is_err());
    assert_eq!(active.queue.len(), 2, "unaccepted head stays queued");
    assert_eq!(
        active.accept_turn_delivery(first_delivery).unwrap().text,
        "first"
    );
    assert_eq!(active.queue.len(), 1);
    assert_eq!(active.queue[0].text, "second");

    active.turn_in_flight = false;
    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let second_delivery = match receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) if text == "second" => delivery_id,
        other => panic!("expected second SendTurn, got {other:?}"),
    };
    active.accept_turn_delivery(second_delivery).unwrap();
    assert!(active.queue.is_empty());
}

#[test]
fn moved_queue_entries_dispatch_in_their_new_order() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    let first = active.push_queued("first".into(), Vec::new());
    active.push_queued("second".into(), Vec::new());
    let third = active.push_queued("third".into(), Vec::new());
    active.move_queued(third, Some(first));
    active.move_queued(first, None);

    let mut sent = Vec::new();
    while active.dispatch_next_pending() == Ok(true) {
        let Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) = receiver.try_recv()
        else {
            panic!("expected SendTurn");
        };
        sent.push(text);
        active.accept_turn_delivery(delivery_id).unwrap();
        active.turn_in_flight = false;
    }
    assert_eq!(sent, ["third", "second", "first"]);
}

#[test]
fn future_scheduled_head_does_not_block_ordinary_dispatch_or_acceptance() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    let scheduled_id = active.push_scheduled(
        "later".into(),
        Vec::new(),
        SystemTime::now() + Duration::from_secs(3_600),
    );
    let ordinary_id = active.push_queued("now".into(), Vec::new());

    assert_eq!(active.dispatch_next_pending(), Ok(true));
    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionCommand::SendTurn {
            delivery_id,
            text,
            ..
        }) if delivery_id == ordinary_id && text == "now"
    ));
    let accepted = active.accept_turn_delivery(ordinary_id).unwrap();
    assert_eq!(accepted.text, "now");
    assert_eq!(active.queue.len(), 1);
    assert_eq!(active.queue[0].id, scheduled_id);
}

#[test]
fn schedule_status_and_queue_actions_preserve_or_remove_deadlines() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scheduled-status-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, receiver) = smol::channel::unbounded();
    let fire_at = now_secs() + 3_600;

    state.update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "scheduled-active".into();
        state.install_selected(active);
        state.schedule_turn(
            "scheduled-active",
            "scheduled".into(),
            Vec::new(),
            fire_at,
            cx,
        );

        let status = state.session_status_snapshot("scheduled-active").unwrap();
        assert_eq!(status.queued_messages.len(), 1);
        assert_eq!(status.queued_messages[0].fire_at_unix_secs, Some(fire_at));
        assert!(status.queued_messages[0].editable);
        let scheduled_id = status.queued_messages[0].id;

        state.steer_queued("scheduled-active", scheduled_id, cx);
        let status = state.session_status_snapshot("scheduled-active").unwrap();
        assert_eq!(status.queued_messages.len(), 1);
        assert_eq!(status.queued_messages[0].fire_at_unix_secs, None);
        assert!(!status.queued_messages[0].editable);
        assert_eq!(
            status.delivery_in_flight,
            Some(status.queued_messages[0].id)
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { .. })
        ));

        let active = state.selected_session_mut().unwrap();
        active.delivery_in_flight = None;
        active.turn_in_flight = false;
        active.queue.clear();
        let drop_id = active.push_scheduled(
            "drop me".into(),
            Vec::new(),
            SystemTime::now() + Duration::from_secs(7_200),
        );
        state.drop_queued("scheduled-active", drop_id, cx);
        assert!(state.selected_session().unwrap().queue.is_empty());
    });
}

#[test]
fn native_retry_keeps_queue_waiting_and_cancellation_releases_it() {
    for automatic in [true, false] {
        let cx = &mut TestAppContext::default();
        let test_store = TestStore::new("native-retry-queue");
        let state = cx.new_entity(TestClientState::new((*test_store).clone()));
        let (commands, receiver) = smol::channel::unbounded();
        state.update(cx, |state, cx| {
            let mut active = live_session(ProviderKind::Codex, commands);
            active.meta.id = "retry".into();
            if automatic {
                active.push_queued("queued user message".into(), Vec::new());
            }
            state.install_selected(active);
            state.on_event(
                "retry",
                AgentEvent::TurnStarted {
                    turn_id: "failed".into(),
                },
                cx,
            );
            let retry = agent::TurnRetry {
                turn_id: "failed".into(),
                retry_at: automatic.then(|| now_secs() + 10),
            };
            state.on_event(
                "retry",
                AgentEvent::TurnRetry {
                    retry: Some(retry.clone()),
                },
                cx,
            );
            state.on_event(
                "retry",
                AgentEvent::TurnCompleted {
                    turn_id: "failed".into(),
                    status: TurnStatus::Failed,
                    usage: None,
                },
                cx,
            );
            let status = state.session_status_snapshot("retry").unwrap();
            assert_eq!(status.activity.working, automatic);
            assert!(!status.activity.turn_running);
            assert_eq!(status.turn_retry, Some(retry));
            assert!(
                next_command(&receiver).is_err(),
                "queued text must not race the native retry"
            );
            if automatic {
                state.interrupt("retry", cx).unwrap();
                assert!(matches!(
                    next_command(&receiver),
                    Ok(SessionCommand::Interrupt)
                ));
                state.on_event("retry", AgentEvent::TurnRetry { retry: None }, cx);
            } else {
                state
                    .selected_session_mut()
                    .unwrap()
                    .push_queued("queued user message".into(), Vec::new());
                state.dispatch_next_queued("retry", cx).unwrap();
            }
            let Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) = next_command(&receiver)
            else {
                panic!("queued message was not sent")
            };
            assert_eq!(text, "queued user message");
            state.on_event("retry", AgentEvent::TurnAccepted { delivery_id }, cx);
            // A rejected new start has no TurnStarted to clear the previous offer.
            state.on_event(
                "retry",
                AgentEvent::TurnCompleted {
                    turn_id: String::new(),
                    status: TurnStatus::Failed,
                    usage: None,
                },
                cx,
            );
            let status = state.session_status_snapshot("retry").unwrap();
            assert!(status.turn_retry.is_none());
            assert!(!status.activity.working);
            assert!(state.retry_turn("retry", "failed".into()).is_err());
        });
    }
}

#[test]
fn usage_limit_event_schedules_resume_when_enabled_only() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-usage-limit-resume-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let resets_at = now_secs() + 3_600;

    state.update(cx, |state, cx| {
        let (commands, _) = smol::channel::unbounded();
        let mut active = live_session(ProviderKind::ClaudeCode, commands);
        active.meta.id = "resume-enabled".into();
        state.install_selected(active);
        state.on_event(
            "resume-enabled",
            AgentEvent::UsageLimitReached { resets_at },
            cx,
        );

        let queue = &state.selected_session().unwrap().queue;
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].text, tcode_core::session::RESUME_PROMPT);
        assert_eq!(
            queue[0].not_before,
            Some(UNIX_EPOCH + Duration::from_secs(resets_at))
        );

        let (commands, _) = smol::channel::unbounded();
        let mut active = live_session(ProviderKind::ClaudeCode, commands);
        active.meta.id = "resume-disabled".into();
        state.install_selected(active);
        state.settings.resume_on_limit_reset = false;
        state.on_event(
            "resume-disabled",
            AgentEvent::UsageLimitReached { resets_at },
            cx,
        );
        assert!(state.selected_session().unwrap().queue.is_empty());
    });
}

#[test]
fn due_scheduled_message_reenters_the_ordinary_send_path() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scheduled-fire-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "due-active".into();
        active.push_scheduled(
            "due now".into(),
            Vec::new(),
            SystemTime::now() - Duration::from_secs(1),
        );
        state.install_selected(active);

        state.fire_due_scheduled(cx);

        let active = state.selected_session().unwrap();
        assert_eq!(active.queue.len(), 1);
        assert_eq!(active.queue[0].text, "due now");
        assert_eq!(active.queue[0].not_before, None);
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { text, .. }) if text == "due now"
        ));
    });
}

#[test]
fn profile_switch_within_one_provider_requires_a_relay() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-profile-relay-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, _receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::ClaudeCode, commands);
        active.meta.id = "profile-relay".into();
        active.meta.model = Some("claude-opus-5".into());
        active.timeline.apply_at(
            None,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "user-1".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "hello".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
        );
        active.timeline.apply_at(
            None,
            &AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        );
        state.install_selected(active);

        // Same ProviderKind, different profile: a different backend. The
        // selection must park behind the relay confirmation and rebind the
        // session's profile so the next launch uses the new endpoint.
        state.set_active_model(
            "profile-relay",
            ProviderKind::ClaudeCode,
            Some("kimi-k3".into()),
            Some("kimi".into()),
            cx,
        );
        let active = state.selected_session().unwrap();
        assert_eq!(active.meta.profile_id.as_deref(), Some("kimi"));
        assert_eq!(active.meta.model.as_deref(), Some("kimi-k3"));
        let pending = active.pending_relay.as_ref().expect("relay pending");
        assert_eq!(pending.from_provider, ProviderKind::ClaudeCode);
        assert_eq!(pending.from_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(pending.from_profile, None);
        assert_eq!(
            state.relay_confirmation("profile-relay"),
            Some(("Claude Code".into(), "kimi".into()))
        );

        // Returning to the original profile cancels the pending relay.
        state.set_active_model(
            "profile-relay",
            ProviderKind::ClaudeCode,
            Some("claude-opus-5".into()),
            None,
            cx,
        );
        let active = state.selected_session().unwrap();
        assert!(active.pending_relay.is_none());
        assert_eq!(active.meta.profile_id, None);
        assert!(state.relay_confirmation("profile-relay").is_none());
    });
}

/// Switching provider while the only turn is still streaming keeps what it
/// produced: the switch arms a relay instead of silently dropping the cursor,
/// and confirming it closes the abandoned turn so the handoff opens a new one.
#[test]
fn provider_switch_mid_turn_relays_the_partial_answer() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-mid-turn-relay-test");
    let store = (*test_store).clone();
    let state = cx.new_entity({
        let mut state = TestClientState::new(store);
        state.set_provider_launcher_for_test(ProviderLauncher(Arc::new(|_, _| {
            Box::pin(std::future::pending())
        })));
        state
    });
    let (commands, _receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "mid-turn-relay".into();
        active.turn_in_flight = true;
        for event in [
            AgentEvent::TurnStarted {
                turn_id: "codex-turn".into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: "user-1".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "compare them".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            AgentEvent::ItemStarted(ThreadItem {
                id: "assistant-1".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "They differ in".into(),
                    summarized: false,
                },
            }),
        ] {
            active.timeline.apply_at(None, &event);
        }
        state.sessions.push(active.meta.clone());
        state.install_selected(active);

        state.set_active_model(
            "mid-turn-relay",
            ProviderKind::ClaudeCode,
            Some("claude-opus-5-5".into()),
            None,
            cx,
        );
        let active = state.selected_session().unwrap();
        assert!(active.pending_relay.is_some());
        assert!(matches!(active.runtime, Runtime::Live(_)));
        assert_eq!(
            state.relay_confirmation("mid-turn-relay"),
            Some(("Codex".into(), "Claude Code".into()))
        );

        state.confirm_relay_and_send("mid-turn-relay", "now review it".into(), Vec::new(), cx);
        let active = state.selected_session().unwrap();
        assert!(active.pending_relay.is_none());
        assert_eq!(
            active.timeline.turns[0].status,
            Some(TurnStatus::Interrupted)
        );
        let relay = active
            .timeline
            .entries
            .iter()
            .find(|entry| matches!(entry.content, EntryContent::ProviderRelay { .. }))
            .expect("relay entry");
        assert_eq!(relay.turn, 1);
        let transcript = active.queue[0].relay_transcript.as_deref().unwrap();
        assert!(transcript.contains("They differ in"));
        assert!(transcript.contains("still running when the conversation was handed over"));
        // The timeline discloses the handoff exactly as it goes out.
        let EntryContent::ProviderRelay { prompt, .. } = &relay.content else {
            unreachable!();
        };
        assert_eq!(
            prompt.as_deref(),
            Some(active.queue[0].wire_text(&[]).as_str())
        );
    });
}

/// One provider process a test launched: the options it started with, and its command and
/// event channels.
type Launch = (
    SessionOptions,
    smol::channel::Receiver<SessionCommand>,
    smol::channel::Sender<AgentEvent>,
);
type Launches = Arc<std::sync::Mutex<Vec<Launch>>>;

/// A launcher that records every start and gives each its own channels.
fn recording_launcher() -> (ProviderLauncher, Launches) {
    let launches = Arc::new(std::sync::Mutex::new(Vec::<Launch>::new()));
    let recorded = launches.clone();
    let launcher = ProviderLauncher(Arc::new(move |provider, options: SessionOptions| {
        let (commands, command_rx) = smol::channel::unbounded();
        let (event_tx, events) = smol::channel::unbounded();
        recorded
            .lock()
            .unwrap()
            .push((options, command_rx, event_tx));
        Box::pin(async move {
            Ok(SessionHandle {
                provider,
                commands,
                events,
            })
        })
    }));
    (launcher, launches)
}

/// Run `command` through the pipe as a client would, and return its acknowledgement.
fn acked(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    command: Command,
) -> Result<CommandResponse, tcode_protocol::ProtocolError> {
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
}

/// Wait for the provider of `launch` to be sent a turn, answering the usage reads before it
/// as [`next_command`] does; the turn's delivery id, text and the channel its events go back
/// on.
fn next_turn(
    cx: &mut TestAppContext,
    launches: &Launches,
    launch: usize,
) -> (u64, String, smol::channel::Sender<AgentEvent>) {
    loop {
        cx.run_until(|_| {
            launches
                .lock()
                .unwrap()
                .get(launch)
                .is_some_and(|launch| !launch.1.is_empty())
        });
        let launches = launches.lock().unwrap();
        let (_, commands, events) = &launches[launch];
        match next_command(commands) {
            Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) => return (delivery_id, text, events.clone()),
            Err(smol::channel::TryRecvError::Empty) => {}
            other => panic!("expected SendTurn, got {other:?}"),
        }
    }
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
    let (launcher, launches) = recording_launcher();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher_for_test(launcher.clone());
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
    cx.run_until(|state| {
        state
            .resident(&session_id)
            .is_some_and(|session| session.turn_in_flight)
    });

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
    cx.run_until(|state| {
        state
            .resident(&session_id)
            .is_some_and(|session| !session.turn_in_flight)
    });

    state.update(cx, |state, cx| {
        let mut worktree = SessionMeta::new(ProviderKind::ClaudeCode, quick.clone(), None);
        worktree.id = "worktree-thread".into();
        worktree.project_id = Some(quick_project.id.clone());
        worktree.worktree = Some(WorktreeInfo {
            root_project_path: quick.clone(),
            base: "main".into(),
            branch: "tcode/worktree-thread".into(),
        });
        state.persist_meta(&worktree, cx);
    });
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
    assert!(
        refusal
            .message
            .contains("move the worktree's branch instead")
    );

    acked(&state, cx, 6, move_to_demo()).unwrap();
    state.read(|state| {
        let session = state.resident(&session_id).unwrap();
        assert!(matches!(session.runtime, Runtime::Idle));
        let meta = state.find_meta(&session_id).unwrap();
        assert_eq!(meta.project_id.as_deref(), Some(demo_id.as_str()));
        assert_eq!(meta.cwd, demo.join("app"));
        assert_eq!(meta.resume_cursor, None);
    });
    cx.run_until_parked();
    drop(state);

    // A host restarted after the move keeps the thread where it went, with its transcript.
    // It is another process, which nothing the stopped host left running reaches.
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher_for_test(launcher);
        state
    });
    state.read(|state| {
        let meta = state.find_meta(&session_id).unwrap();
        assert_eq!(meta.project_id.as_deref(), Some(demo_id.as_str()));
        assert_eq!(meta.cwd, demo.join("app"));
    });
    state.update(cx, |state, cx| state.select_session(&session_id, cx));
    cx.run_until(|state| {
        state
            .selected_session()
            .is_some_and(|session| session.timeline.turns.len() == 1)
    });
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
    state.read(|state| {
        let session = state.selected_session().unwrap();
        let relay = session
            .timeline
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
        assert!(session.meta.moved_from.is_none());
    });
}

/// A moved thread's first message hands the provider the conversation in the host's
/// log even when the provider is up before the host has read that log: the message
/// waits for the read, and the thread forgets where it moved from only once the
/// handoff went out.
#[test]
fn a_moved_threads_handoff_waits_for_its_log() {
    let test_store = TestStore::new("tcode-handoff-waits-for-log");
    let quick = test_store.root().join("quick");
    let demo = test_store.root().join("demo");
    std::fs::create_dir_all(&quick).unwrap();
    std::fs::create_dir_all(&demo).unwrap();
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, demo.clone(), None);
    meta.id = "thread".into();
    meta.moved_from = Some(quick.clone());
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
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, provider) = smol::channel::unbounded();
    // Nothing the host hands off its own thread lands before the test runs the host,
    // so the thread's log is still being read while its provider is up and the
    // message is sent.
    state.update(cx, |state, cx| {
        state.select_session("thread", cx);
        let thread = state.selected_session_mut().unwrap();
        thread.runtime = Runtime::Live(commands);
        thread.live_cwd = thread.meta.cwd.clone();
    });
    state.dispatch_command(
        cx,
        1,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "what did we decide?".into(),
            attachment_paths: Vec::new(),
        },
    );
    assert!(
        next_command(&provider).is_err(),
        "the message waits for the log"
    );
    state.read(|state| {
        assert_eq!(
            state.find_meta("thread").unwrap().moved_from,
            Some(quick.clone())
        );
    });

    let SessionCommand::SendTurn { text, .. } = cx.run_until_command(&provider) else {
        panic!("the message goes out once the log is read");
    };
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
    state.read(|state| assert!(state.find_meta("thread").unwrap().moved_from.is_none()));
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
        state.set_provider_launcher_for_test(launcher);
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

    acked(&state, cx, 1, move_to(&demo_project)).unwrap();
    state.read(|state| {
        assert_eq!(
            state.find_meta("thread").unwrap().resume_cursor,
            Some(native.clone())
        );
    });
    state.update(cx, |state, cx| state.select_session("thread", cx));
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
    cx.run_until(|state| {
        state
            .resident("thread")
            .is_some_and(|session| session.timeline.turns.len() == 2 && !session.has_work())
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
    cx.run_until(|state| {
        state
            .resident("thread")
            .is_some_and(|session| session.timeline.turns.len() == 3 && !session.has_work())
    });
    acked(&state, cx, 5, send("go on")).unwrap();
    let (_, text, _) = next_turn(cx, &launches, 0);
    assert_eq!(text, "go on");
    assert_eq!(
        launches.lock().unwrap().len(),
        1,
        "the same provider process"
    );
    state.read(|state| {
        let session = state.resident("thread").unwrap();
        assert_eq!(session.live_cwd, elsewhere);
        assert_eq!(session.meta.cwd, elsewhere);
        assert_eq!(state.find_meta("thread").unwrap().cwd, elsewhere);
        assert_eq!(session.meta.moved_from, None);
        let moves: Vec<_> = session
            .timeline
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
    });
}

/// The next command the provider of `launch` is sent that is not a turn.
fn next_worktree_answer(
    cx: &mut TestAppContext,
    launches: &Launches,
    launch: usize,
) -> (String, agent::WorktreeAnswer) {
    loop {
        cx.run_until(|_| !launches.lock().unwrap()[launch].1.is_empty());
        match next_command(&launches.lock().unwrap()[launch].1) {
            Ok(SessionCommand::RespondWorktree { request_id, answer }) => {
                return (request_id, answer);
            }
            Err(smol::channel::TryRecvError::Empty) => {}
            other => panic!("expected RespondWorktree, got {other:?}"),
        }
    }
}

/// A worktree Claude Code asks for comes from Tcode's worktree module, in the data
/// directory's bucket for the repository, and belongs to the thread; asked for by the
/// same name again it is the same worktree. The thread follows the native session into
/// it and out of it with the same provider process and no handoff, and Claude's removal
/// removes it and its untouched branch.
#[test]
fn a_claude_worktree_comes_from_tcode_and_the_thread_follows_its_session() {
    use agent::{WorktreeAnswer, WorktreeRequest};
    let test_store = TestStore::new("tcode-claude-worktree");
    let repo = test_store.root().join("repo");
    init_git_repo(&repo);
    let project = Project::from_root(repo.clone());
    test_store.upsert_project(&project).unwrap();
    let native = agent::ResumeCursor(serde_json::json!({"session_id": "native-1"}));
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, repo.clone(), None);
    meta.id = "thread".into();
    meta.project_id = Some(project.id.clone());
    meta.resume_cursor = Some(native.clone());
    test_store.upsert_meta(&meta).unwrap();
    let cx = &mut TestAppContext::default();
    let (launcher, launches) = recording_launcher();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher_for_test(launcher);
        state
    });
    state.update(cx, |state, cx| state.select_session("thread", cx));
    let send = |text: &str| Command::SendTurn {
        session_id: "thread".into(),
        text: text.into(),
        attachment_paths: Vec::new(),
    };
    acked(&state, cx, 1, send("work in a worktree")).unwrap();
    let (delivery_id, _, events) = next_turn(cx, &launches, 0);
    for event in [
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
    ] {
        events.try_send(event).unwrap();
    }
    let create = WorktreeRequest::Create {
        cwd: repo.clone(),
        name: "feat".into(),
    };
    events
        .try_send(AgentEvent::WorktreeRequested {
            request_id: "hook-1".into(),
            request: create.clone(),
        })
        .unwrap();
    let (request_id, answer) = next_worktree_answer(cx, &launches, 0);
    assert_eq!(request_id, "hook-1");
    let WorktreeAnswer::Ready {
        path: worktree,
        reused: false,
    } = answer.clone()
    else {
        panic!("{answer:?}")
    };
    let bucket = tcode_services::worktree::worktrees_root(test_store.root())
        .canonicalize()
        .unwrap();
    assert_eq!(worktree.parent().unwrap().parent().unwrap(), bucket);
    assert_eq!(worktree.file_name().unwrap(), "feat");
    assert_eq!(
        run_git(&worktree, &["branch", "--show-current"])
            .unwrap()
            .trim(),
        "tcode/feat"
    );
    state.read(|state| {
        let meta = state.find_meta("thread").unwrap();
        assert_eq!(meta.cwd, repo);
        assert_eq!(
            meta.other_worktrees
                .iter()
                .map(|owned| (&owned.path, owned.info.branch.as_str()))
                .collect::<Vec<_>>(),
            [(&worktree, "tcode/feat")]
        );
    });
    for event in [
        AgentEvent::WorktreeResolved {
            request_id: "hook-1".into(),
            request: create.clone(),
            answer,
        },
        AgentEvent::SessionMoved {
            from: repo.clone(),
            to: worktree.clone(),
            parent_item_id: None,
        },
        AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: TurnStatus::Completed,
            usage: None,
        },
    ] {
        events.try_send(event).unwrap();
    }
    cx.run_until(|state| {
        state
            .resident("thread")
            .is_some_and(|session| session.meta.cwd == worktree && !session.has_work())
    });
    state.read(|state| {
        let session = state.resident("thread").unwrap();
        assert_eq!(session.live_cwd, worktree);
        assert_eq!(
            session.meta.worktree.as_ref().map(|w| w.branch.as_str()),
            Some("tcode/feat")
        );
        assert_eq!(session.meta.other_worktrees, []);
        assert_eq!(session.meta.moved_from, None);
        assert_eq!(session.meta.resume_cursor, Some(native.clone()));
        let shown: Vec<_> = session
            .timeline
            .entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::Worktree { request, answer } => {
                    Some(format!("{request:?} {answer:?}"))
                }
                EntryContent::SessionMoved { from, to } => Some(format!("{from:?} -> {to:?}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            shown,
            [
                format!(
                    "{create:?} {:?}",
                    WorktreeAnswer::Ready {
                        path: worktree.clone(),
                        reused: false
                    }
                ),
                format!("{repo:?} -> {worktree:?}"),
            ]
        );
    });

    acked(&state, cx, 2, send("go on")).unwrap();
    let (delivery_id, text, events) = next_turn(cx, &launches, 0);
    assert_eq!(text, "go on");
    assert_eq!(
        launches.lock().unwrap().len(),
        1,
        "the same provider process"
    );
    state.read(|state| {
        let session = state.resident("thread").unwrap();
        assert!(
            !session
                .timeline
                .entries
                .iter()
                .any(|entry| matches!(entry.content, EntryContent::ProviderRelay { .. }))
        );
    });
    events
        .try_send(AgentEvent::TurnAccepted { delivery_id })
        .unwrap();
    events
        .try_send(AgentEvent::WorktreeRequested {
            request_id: "hook-2".into(),
            request: create,
        })
        .unwrap();
    assert_eq!(
        next_worktree_answer(cx, &launches, 0),
        (
            "hook-2".into(),
            WorktreeAnswer::Ready {
                path: worktree.clone(),
                reused: true
            }
        )
    );

    events
        .try_send(AgentEvent::SessionMoved {
            from: worktree.clone(),
            to: repo.clone(),
            parent_item_id: None,
        })
        .unwrap();
    events
        .try_send(AgentEvent::WorktreeRequested {
            request_id: "hook-3".into(),
            request: WorktreeRequest::Remove {
                path: worktree.clone(),
            },
        })
        .unwrap();
    assert_eq!(
        next_worktree_answer(cx, &launches, 0),
        ("hook-3".into(), WorktreeAnswer::Removed)
    );
    assert!(!worktree.exists());
    assert_eq!(
        run_git(&repo, &["branch", "--list", "tcode/feat"]).unwrap(),
        ""
    );
    state.read(|state| {
        let meta = state.find_meta("thread").unwrap();
        assert_eq!(meta.cwd, repo);
        assert_eq!(meta.worktree, None);
        assert_eq!(meta.other_worktrees, []);
    });
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
        state.set_provider_launcher_for_test(launcher);
        state
    });
    state.update(cx, |state, cx| state.select_session("thread", cx));
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
    cx.run_until(|state| {
        state
            .sessions
            .iter()
            .any(|meta| meta.native_subagent.is_some() && meta.cwd == worktree)
    });
    state.read(|state| {
        let thread = state.find_meta("thread").unwrap();
        assert_eq!(thread.cwd, repo, "the session stays where it was");
        assert_eq!(thread.other_worktrees.len(), 1);
    });

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

/// Two Claude accounts, each signed in through its own config directory, an
/// Anthropic-compatible endpoint profile, and a thread whose first turn ran on the built-in
/// account under the native session `native-1`.
struct AccountSwitch {
    _store: TestStore,
    state: TestEntity,
    launches: Launches,
    session_id: String,
    default_home: PathBuf,
    work_home: PathBuf,
}

/// The project directory the CLI keeps the thread's sessions under: its own encoding of the
/// working directory, which Tcode reuses rather than derives.
const CLI_PROJECT_DIR: &str = "-tmp-release-project-3f2a";

impl AccountSwitch {
    /// With `on_disk`, the built-in account's home holds the session as the CLI writes it:
    /// the transcript, the subagent transcripts and stored tool results beside it, its file
    /// checkpoints, task list, todo lists, hook environment and the plans of the slug its
    /// transcript records, next to another session of the same project with its own.
    fn new(cx: &mut TestAppContext, name: &str, on_disk: bool) -> Self {
        let store = TestStore::new(name);
        let cwd = store.root().join("release");
        let default_home = store.root().join("claude-default");
        let work_home = store.root().join("claude-work");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = Project::from_root(cwd.clone());
        store.upsert_project(&project).unwrap();
        let (launcher, launches) = recording_launcher();
        // No `claude` runs here: a usage read the scripted provider leaves unanswered
        // would otherwise start the CLI.
        let binary = Some(store.root().join("no-claude"));
        let mut host = TestClientState::new((*store).clone());
        host.set_provider_launcher_for_test(launcher);
        let builtin = host.settings.provider_mut(ProviderKind::ClaudeCode);
        builtin.home_path = Some(default_home.clone());
        builtin.binary_path = binary.clone();
        for (id, home, env) in [
            ("work", &work_home, Vec::new()),
            (
                "proxy",
                &store.root().join("claude-proxy"),
                vec![EnvVar {
                    name: "ANTHROPIC_BASE_URL".into(),
                    value: "https://llm-proxy.example/anthropic".into(),
                    sensitive: false,
                }],
            ),
        ] {
            host.settings.profiles.insert(
                id.into(),
                ProviderProfile {
                    kind: ProviderKind::ClaudeCode,
                    settings: ProviderSettings {
                        display_name: Some(id.into()),
                        env,
                        home_path: Some(home.clone()),
                        binary_path: binary.clone(),
                        ..ProviderSettings::default()
                    },
                },
            );
        }
        let state = cx.new_entity(host);

        let Ok(CommandResponse::SessionId(Some(session_id))) = acked(
            &state,
            cx,
            1,
            Command::StartDraft {
                project_id: project.id.clone(),
                cwd,
            },
        ) else {
            panic!("draft not started");
        };
        let switch = Self {
            _store: store,
            state,
            launches,
            session_id,
            default_home,
            work_home,
        };
        switch.select(cx, 2, None).unwrap();
        switch.send(cx, 3, "plan the release").unwrap();
        let (delivery_id, _, events) = next_turn(cx, &switch.launches, 0);
        if on_disk {
            let project = Path::new("projects").join(CLI_PROJECT_DIR);
            for (path, content) in [
                (
                    project.join("native-1.jsonl"),
                    "{\"turn\":1}\n{\"turn\":1,\"slug\":\"quiet-release-plan\"}\n",
                ),
                (
                    project.join("native-1/subagents/agent-a.jsonl"),
                    "{\"agent\":\"a\"}\n",
                ),
                (project.join("native-1/subagents/agent-a.meta.json"), "{}"),
                (
                    project.join("native-1/tool-results/toolu_1.txt"),
                    "release notes",
                ),
                (
                    project.join("other-session.jsonl"),
                    "{\"other\":true,\"slug\":\"other-plan\"}\n",
                ),
                ("file-history/native-1/9c1e@v1".into(), "before"),
                ("tasks/native-1/1.json".into(), "{\"subject\":\"Tag v1\"}"),
                (
                    "session-env/native-1/sessionstart-hook-0.sh".into(),
                    "export STAGE=release\n",
                ),
                ("todos/native-1-agent-native-1.json".into(), "[]"),
                ("todos/native-1-agent-a.json".into(), "[]"),
                ("plans/quiet-release-plan.md".into(), "# Release"),
                ("plans/quiet-release-plan-agent-a.md".into(), "# Notes"),
                ("tasks/other-session/1.json".into(), "{}"),
                ("todos/other-session-agent-other-session.json".into(), "[]"),
                ("plans/other-plan.md".into(), "# Other"),
            ] {
                let path = switch.default_home.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            }
        }
        switch.run_turn(cx, &events, delivery_id, "Tag v1 on Friday.");
        switch
    }

    fn select(
        &self,
        cx: &mut TestAppContext,
        id: u64,
        profile_id: Option<&str>,
    ) -> Result<CommandResponse, tcode_protocol::ProtocolError> {
        acked(
            &self.state,
            cx,
            id,
            Command::SetActiveModel {
                session_id: self.session_id.clone(),
                provider: ProviderKind::ClaudeCode,
                model: None,
                profile_id: profile_id.map(str::to_owned),
            },
        )
    }

    fn send(
        &self,
        cx: &mut TestAppContext,
        id: u64,
        text: &str,
    ) -> Result<CommandResponse, tcode_protocol::ProtocolError> {
        acked(
            &self.state,
            cx,
            id,
            Command::SendTurn {
                session_id: self.session_id.clone(),
                text: text.into(),
                attachment_paths: Vec::new(),
            },
        )
    }

    /// Answer the turn the provider was sent, as Claude Code does on `native-1`.
    fn run_turn(
        &self,
        cx: &mut TestAppContext,
        events: &smol::channel::Sender<AgentEvent>,
        delivery_id: u64,
        answer: &str,
    ) {
        for event in [
            AgentEvent::SessionStarted {
                provider_session_id: "native-1".into(),
                resume: agent::ResumeCursor(serde_json::json!({ "session_id": "native-1" })),
                model: None,
            },
            AgentEvent::TurnAccepted { delivery_id },
            AgentEvent::TurnStarted {
                turn_id: format!("turn-{delivery_id}"),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("assistant-{delivery_id}"),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: answer.into(),
                    summarized: false,
                },
            }),
            AgentEvent::TurnCompleted {
                turn_id: format!("turn-{delivery_id}"),
                status: TurnStatus::Completed,
                usage: None,
            },
        ] {
            events.try_send(event).unwrap();
        }
        let session_id = self.session_id.clone();
        cx.run_until(|state| {
            state
                .resident(&session_id)
                .is_some_and(|session| !session.turn_in_flight && session.queue.is_empty())
        });
    }

    /// The config directory and resume cursor launch `launch` started with.
    fn launched(&self, launch: usize) -> (Option<PathBuf>, Option<agent::ResumeCursor>) {
        let launches = self.launches.lock().unwrap();
        let options = &launches[launch].0;
        (options.launch_env.home.clone(), options.resume.clone())
    }

    fn relays(&self) -> Vec<Option<String>> {
        self.state.read(|state| {
            state
                .resident(&self.session_id)
                .unwrap()
                .timeline
                .entries
                .iter()
                .filter_map(|entry| match &entry.content {
                    EntryContent::ProviderRelay { prompt, .. } => Some(prompt.clone()),
                    _ => None,
                })
                .collect()
        })
    }
}

fn native_session() -> Option<agent::ResumeCursor> {
    Some(agent::ResumeCursor(
        serde_json::json!({ "session_id": "native-1" }),
    ))
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

/// Park `id` with a live provider running its turn, as a thread is whose log
/// the host let go of while it worked.
fn park_running(state: &TestEntity, cx: &mut TestAppContext, id: &str) {
    state.update(cx, |state, _| {
        let mut running = live_session(ProviderKind::ClaudeCode, smol::channel::unbounded().0);
        running.meta = state.find_meta(id).unwrap();
        running.turn_in_flight = true;
        state.residents.parked.insert(id.into(), running);
    });
}

/// A live session with `provider`, nothing queued, no turn in flight.
fn live_session(
    provider: ProviderKind,
    commands: smol::channel::Sender<SessionCommand>,
) -> ActiveSession {
    ActiveSession {
        runtime: Runtime::Live(commands),
        ..ActiveSession::new(
            SessionMeta::new(provider, PathBuf::from("/tmp/project"), None),
            false,
            Vec::new(),
        )
    }
}

#[test]
fn provider_config_options_remove_retired_wire_controls() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-provider-config-retirement");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        let (commands, _receiver) = smol::channel::unbounded();
        let mut session = live_session(ProviderKind::Acp, commands);
        session.meta.id = "config-session".into();
        state.sessions.push(session.meta.clone());
        state.install_selected(session);
        state.on_event(
            "config-session",
            AgentEvent::ProviderOptions {
                descriptors: vec![OptionDescriptor::Boolean {
                    id: "acp:config:thinking".into(),
                    label: "Thinking".into(),
                    default_value: true,
                    role: Default::default(),
                    apply: Default::default(),
                    recommended: None,
                    permissive: None,
                }],
                selections: vec![OptionSelection {
                    id: "acp:config:thinking".into(),
                    value: serde_json::json!(true),
                }],
            },
            cx,
        );
        assert_eq!(
            state
                .session_status_snapshot("config-session")
                .unwrap()
                .provider_option_descriptors
                .len(),
            1
        );
        state.on_event(
            "config-session",
            AgentEvent::ProviderOptions {
                descriptors: Vec::new(),
                selections: Vec::new(),
            },
            cx,
        );
        let removed = state.session_status_snapshot("config-session").unwrap();
        assert!(removed.provider_option_descriptors.is_empty());
        assert!(removed.provider_option_selections.is_empty());
        assert!(removed.provider_option_requested_selections.is_empty());
    });
}

/// A new thread starts at what its project names, else at what its Harness
/// names, else at the permission Tcode recommends for that Harness.
#[test]
fn a_new_threads_permission_is_its_projects_then_its_harness_then_the_recommended() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-permission-default");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let value = |state: &AppState| {
        state
            .initial_permission_selection(Some("project"), ProviderKind::ClaudeCode)
            .unwrap()
            .value
    };
    state.update(cx, |state, _| {
        let mut project = tcode_core::project::Project::from_root("/project".into());
        project.id = "project".into();
        state.projects.push(project);
        assert_eq!(value(state), "auto");
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .default_permission = Some("acceptEdits".into());
        assert_eq!(value(state), "acceptEdits");
        state.projects[0]
            .permission_defaults
            .insert("claude".into(), "default".into());
        assert_eq!(value(state), "default");
    });
}

#[test]
fn native_permission_selection_stays_pending_until_provider_confirmation() {
    for (provider, id, initial, requested, restart) in [
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
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-permission-confirmation");
        let state = cx.new_entity(TestClientState::new((*store).clone()));
        let (commands, receiver) = smol::channel::unbounded();
        state.update(cx, |state, cx| {
            let mut session = live_session(provider, commands);
            session.meta.id = "permission-session".into();
            state.sessions.push(session.meta.clone());
            state.install_selected(session);
            let confirmed = |value| AgentEvent::ProviderOptions {
                descriptors: vec![permission_control(provider).unwrap()],
                selections: vec![OptionSelection { id: id.into(), value }],
            };
            state.on_event("permission-session", confirmed(initial.clone()), cx);
            assert!(!state.session_status_snapshot("permission-session").unwrap().options_pending_restart);
            state.set_active_option("permission-session", id, Some(requested.clone()), cx);
            if restart {
                assert!(receiver.try_recv().is_err());
            } else {
                assert!(matches!(receiver.try_recv(), Ok(SessionCommand::SetOption { id: sent_id, value }) if sent_id == id && value == requested));
            }
            state.on_event("permission-session", confirmed(initial.clone()), cx);
            let pending = state.session_status_snapshot("permission-session").unwrap();
            assert_eq!(pending.provider_option_selections.iter().find(|selection| selection.id == id).unwrap().value, initial);
            assert_eq!(pending.provider_option_requested_selections.iter().find(|selection| selection.id == id).unwrap().value, requested);
            assert_eq!(pending.options_pending_restart, restart);
            assert!(matches!(state.resident("permission-session").unwrap().runtime, Runtime::Live(_)));
            if !restart {
                state.on_event("permission-session", confirmed(requested.clone()), cx);
                let applied = state.session_status_snapshot("permission-session").unwrap();
                assert_eq!(applied.provider_option_selections.iter().find(|selection| selection.id == id).unwrap().value, requested);
                assert_eq!(applied.provider_option_requested_selections, applied.provider_option_selections);
                assert!(!applied.options_pending_restart);
            }
            assert!(receiver.try_recv().is_err());
        });
    }
}

#[test]
fn effort_changes_use_per_turn_overrides_or_require_provider_restart() {
    for (provider, effort, restart) in [
        (ProviderKind::Codex, Some("high"), false),
        (ProviderKind::OpenCode, Some("high"), false),
        (ProviderKind::ClaudeCode, None, true),
        (ProviderKind::Pi, None, true),
        (ProviderKind::Acp, None, false),
    ] {
        let (commands, receiver) = smol::channel::unbounded();
        let mut active = live_session(provider, commands);
        active.meta.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("high"),
        });
        assert_eq!(active.options_changed_while_live(), restart, "{provider:?}");
        if restart {
            active.shutdown_to_idle();
            assert!(matches!(receiver.try_recv(), Ok(SessionCommand::Shutdown)));
        } else {
            active.push_queued("next turn".into(), Vec::new());
            assert_eq!(active.dispatch_next_pending(), Ok(true));
            let SessionCommand::SendTurn {
                options: Some(options),
                ..
            } = receiver.try_recv().unwrap()
            else {
                panic!("expected per-turn options for {provider:?}");
            };
            assert_eq!(options.effort.as_deref(), effort, "{provider:?}");
        }
    }
}

#[test]
fn queued_message_stamps_live_context_window_change() {
    let mut active = live_session(ProviderKind::ClaudeCode, smol::channel::unbounded().0);
    active.meta.model = Some("claude-opus-5".into());
    active.live_option_selections.push(OptionSelection {
        id: "contextWindow".into(),
        value: serde_json::json!("1m"),
    });
    active.meta.option_selections.push(OptionSelection {
        id: "contextWindow".into(),
        value: serde_json::json!(500_000),
    });

    active.push_queued("queued".into(), Vec::new());
    active.stamp_context_window_change(None);
    assert_eq!(active.queue[0].context_window_changed, Some(500_000));

    // Choosing the model's default preset while the provider already runs at
    // it changes nothing.
    active.live_option_selections.clear();
    active.meta.option_selections.clear();
    active.meta.option_selections.push(OptionSelection {
        id: "contextWindow".into(),
        value: serde_json::json!("1m"),
    });
    active.push_queued("default".into(), Vec::new());
    active.stamp_context_window_change(Some(1_000_000));
    assert_eq!(active.queue[1].context_window_changed, None);

    active.runtime = Runtime::Idle;
    active.push_scheduled("idle".into(), Vec::new(), SystemTime::now());
    active.stamp_context_window_change(Some(1_000_000));
    assert_eq!(active.queue[2].context_window_changed, None);
}

#[test]
fn shutdown_all_notifies_active_and_parked_live_providers() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-app-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    let (active_commands, active_receiver) = smol::channel::unbounded();
    let (parked_commands, parked_receiver) = smol::channel::unbounded();
    let parked = live_session(ProviderKind::ClaudeCode, parked_commands);
    let (other_commands, other_receiver) = smol::channel::unbounded();
    let other = live_session(ProviderKind::Acp, other_commands);
    state.update(cx, |state, cx| {
        state.install_selected(live_session(ProviderKind::Codex, active_commands));
        state
            .residents
            .parked
            .insert(parked.meta.id.clone(), parked);
        state.residents.parked.insert(other.meta.id.clone(), other);
        state.shutdown_all(cx);

        assert!(matches!(
            active_receiver.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(matches!(
            parked_receiver.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(matches!(
            other_receiver.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(state.selected_session().is_none());
        assert!(state.residents.parked.is_empty());
    });
}

/// Enter always queues while a turn runs; ⌘Enter steers only where the
/// provider actually supports it, and otherwise degrades to queueing.
#[test]
fn send_routing_matrix() {
    let (commands, _rx) = smol::channel::unbounded();
    let mut codex = live_session(ProviderKind::Codex, commands.clone());

    // Idle: both gestures are a plain send — there is nothing to steer into.
    assert_eq!(codex.route(false), SendRouting::Send);
    assert_eq!(codex.route(true), SendRouting::Send);

    // Turn running: Enter queues, ⌘Enter steers (Codex has `turn/steer`).
    codex.turn_in_flight = true;
    assert_eq!(codex.route(false), SendRouting::Queue);
    assert_eq!(codex.route(true), SendRouting::Steer);

    let mut claude = live_session(ProviderKind::ClaudeCode, commands.clone());
    claude.turn_in_flight = true;
    assert_eq!(claude.route(true), SendRouting::Steer);

    let mut pi = live_session(ProviderKind::Pi, commands.clone());
    pi.turn_in_flight = true;
    assert_eq!(pi.route(true), SendRouting::Steer);

    let mut opencode = live_session(ProviderKind::OpenCode, commands.clone());
    opencode.turn_in_flight = true;
    assert_eq!(opencode.route(true), SendRouting::Steer);

    // ACP has no steering method, so a steer must fall back to the queue
    // rather than silently vanish.
    let mut acp = live_session(ProviderKind::Acp, commands);
    acp.turn_in_flight = true;
    assert_eq!(acp.route(false), SendRouting::Queue);
    assert_eq!(acp.route(true), SendRouting::QueueUnsupported);

    // A provider that can steer still can't while it isn't live.
    let mut dead = live_session(ProviderKind::Codex, smol::channel::unbounded().0);
    dead.runtime = Runtime::Idle;
    dead.turn_in_flight = true;
    assert_eq!(dead.route(true), SendRouting::QueueUnsupported);
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

/// An image-only send keeps its empty text in the transcript (the bubble
/// renders just the thumbnails) while the wire carries a placeholder.
#[test]
fn image_only_message_gets_placeholder_on_the_wire_only() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    let attachment = Attachment {
        media_type: "image/png".into(),
        data_base64: "AAAA".into(),
        source_path: Some("/tmp/a.png".into()),
    };
    active.push_queued(String::new(), vec![attachment.clone()]);

    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let delivery_id = match receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id,
            text,
            attachments,
            ..
        }) => {
            assert_eq!(text, tcode_core::attachments::IMAGE_ONLY_MESSAGE);
            assert_eq!(attachments, vec![attachment]);
            delivery_id
        }
        other => panic!("expected SendTurn, got {other:?}"),
    };
    // The accepted (recorded) message keeps the user's empty text and the
    // local path for the timeline.
    let recorded = active.accept_turn_delivery(delivery_id).unwrap();
    assert_eq!(recorded.text, "");
    assert_eq!(attachment_paths(&recorded.attachments), vec!["/tmp/a.png"]);
}

#[test]
fn relay_context_rides_only_with_the_first_handoff_message() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    active.push_queued("continue here".into(), Vec::new());
    active.queue[0].relay_transcript = Some("# prior work".into());
    active.push_queued("follow up".into(), Vec::new());

    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let first = receiver.try_recv().unwrap();
    let SessionCommand::SendTurn {
        delivery_id, text, ..
    } = first
    else {
        panic!("expected first send turn");
    };
    assert!(text.starts_with(tcode_core::relay::RELAY_PREAMBLE));
    assert!(text.contains("<conversation-transcript>\n# prior work\n</conversation-transcript>"));
    assert!(text.contains("<new-user-message>\ncontinue here\n</new-user-message>"));

    active.accept_turn_delivery(delivery_id).unwrap();
    active.turn_in_flight = false;
    assert_eq!(active.dispatch_next_pending(), Ok(true));
    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionCommand::SendTurn { text, .. }) if text == "follow up"
    ));
}

#[test]
fn startup_generation_rejects_stale_same_session_attempt() {
    for stale_after_live in [false, true] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-startup-generation");
        let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
        meta.id = "same-session".into();
        store.upsert_meta(&meta).unwrap();
        let (starts, started) = smol::channel::unbounded();
        let launcher = ProviderLauncher(Arc::new(move |_, _| {
            let (complete, completed) = smol::channel::bounded(1);
            starts.try_send(complete).unwrap();
            Box::pin(async move { Ok(completed.recv().await.unwrap()) })
        }));
        let state = cx.new_entity({
            let mut state = TestClientState::new((*store).clone());
            state.set_provider_launcher_for_test(launcher);
            state
        });
        state.update(cx, |state, cx| {
            state.select_session("same-session", cx);
            state.send_turn("same-session", "obsolete attempt".into(), Vec::new(), cx);
        });
        cx.run_until(|_| !started.is_empty());
        let first = started.try_recv().unwrap();
        state.update(cx, |state, cx| {
            state.shutdown_active("same-session", cx);
            state.host.select_session("same-session", cx);
            state.send_turn("same-session", "current attempt".into(), Vec::new(), cx);
        });
        cx.run_until(|_| !started.is_empty());
        let second = started.try_recv().unwrap();
        let (stale_commands, stale_actor) = smol::channel::unbounded();
        let (_stale_events, stale_events_rx) = smol::channel::unbounded();
        let stale_handle = SessionHandle {
            provider: ProviderKind::Codex,
            commands: stale_commands,
            events: stale_events_rx,
        };
        let (current_commands, current_actor) = smol::channel::unbounded();
        let (_current_events, current_events_rx) = smol::channel::unbounded();
        let current_handle = SessionHandle {
            provider: ProviderKind::Codex,
            commands: current_commands.clone(),
            events: current_events_rx,
        };
        if stale_after_live {
            second.try_send(current_handle).unwrap();
            cx.run_until(|_| !current_actor.is_empty());
            first.try_send(stale_handle).unwrap();
        } else {
            first.try_send(stale_handle).unwrap();
            cx.run_until(|_| !stale_actor.is_empty());
            state.read(|state| {
                assert!(matches!(
                    state.resident("same-session").unwrap().runtime,
                    Runtime::Starting { .. }
                ))
            });
            second.try_send(current_handle).unwrap();
        }
        cx.run_until(|_| !stale_actor.is_empty() && !current_actor.is_empty());
        assert!(matches!(
            stale_actor.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(stale_actor.try_recv().is_err());
        let delivery_id = match current_actor.try_recv().unwrap() {
            SessionCommand::SendTurn {
                delivery_id, text, ..
            } => {
                assert_eq!(text, "current attempt");
                delivery_id
            }
            other => panic!("expected current delivery, got {other:?}"),
        };
        state.update(cx, |state, cx| {
            state.on_event("same-session", AgentEvent::TurnAccepted { delivery_id }, cx);
            let active = state.resident("same-session").unwrap();
            assert!(active.queue.is_empty());
            assert!(active.turn_in_flight);
            assert!(matches!(&active.runtime, Runtime::Live(commands) if commands.same_channel(&current_commands)));
        });
        assert!(
            current_actor.try_recv().is_err(),
            "stale completion must not restart or duplicate current delivery"
        );
    }
}

#[test]
fn unaccepted_send_survives_eof_and_is_delivered_once_after_resume() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!(
        "tcode-acked-delivery-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-acked-delivery-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (session, first_actor) = fake_live_session(cwd.clone());
    let session_id = session.meta.id.clone();

    state.update(cx, |state, cx| {
        state.install_selected(session);
        // The preceding model turn has completed, but Claude still owns a
        // background process. This is the idle-send window from the repro.
        state.on_event(
            &session_id,
            AgentEvent::TurnStarted {
                turn_id: "background-launch".into(),
            },
            cx,
        );
        state.on_event(
            &session_id,
            AgentEvent::BackgroundTasksChanged { count: 1 },
            cx,
        );
        state.on_event(
            &session_id,
            AgentEvent::TurnCompleted {
                turn_id: "background-launch".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );

        state.send_turn(&session_id, "survive the eof race".into(), Vec::new(), cx);
        let (delivery_id, submitted_text) = match next_command(&first_actor) {
            Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) => (delivery_id, text),
            other => panic!("expected submitted SendTurn, got {other:?}"),
        };
        let active = state.selected_session().unwrap();
        assert_eq!(active.queue.len(), 1);
        assert_eq!(active.delivery_in_flight, Some(delivery_id));
        assert!(
            !state
                .store
                .read_events(&session_id)
                .unwrap()
                .iter()
                .any(|stored| {
                    matches!(
                        &stored.event,
                        AgentEvent::ItemCompleted(ThreadItem {
                            content: ItemContent::UserMessage { text, .. },
                            ..
                        }) if text == "survive the eof race"
                    )
                })
        );

        // EOF wins before the first actor writes, so no TurnAccepted exists.
        state.on_event(
            &session_id,
            AgentEvent::SessionClosed {
                reason: Some("claude closed stdout".into()),
            },
            cx,
        );
        let active = state.selected_session().unwrap();
        assert!(matches!(active.runtime, Runtime::Idle));
        assert_eq!(active.queue.len(), 1);
        assert_eq!(active.delivery_in_flight, None);

        let (resumed_commands, resumed_actor) = smol::channel::unbounded();
        state.selected_session_mut().unwrap().runtime = Runtime::Live(resumed_commands);
        assert_eq!(state.dispatch_next_queued(&session_id, cx), Ok(true));
        let retried_delivery = match next_command(&resumed_actor) {
            Ok(SessionCommand::SendTurn {
                delivery_id: retried_id,
                text,
                ..
            }) if text == submitted_text => retried_id,
            other => panic!("expected retried SendTurn, got {other:?}"),
        };
        assert_eq!(retried_delivery, delivery_id);

        state.on_event(
            &session_id,
            AgentEvent::TurnAccepted {
                delivery_id: retried_delivery,
            },
            cx,
        );
        // A duplicate acceptance cannot remove or persist anything twice.
        state.on_event(
            &session_id,
            AgentEvent::TurnAccepted {
                delivery_id: retried_delivery,
            },
            cx,
        );
        assert!(state.selected_session().unwrap().queue.is_empty());
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let delivered = state
            .store
            .read_events(&session_id)
            .unwrap()
            .iter()
            .filter(|stored| {
                matches!(
                    &stored.event,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::UserMessage { text, .. },
                        ..
                    }) if text == "survive the eof race"
                )
            })
            .count();
        assert_eq!(delivered, 1);
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn inferred_startup_model_updates_live_model_without_restart() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-live-model-sync-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "model-sync".into();

    state.update(cx, |state, cx| {
        state.install_selected(session);
        state.on_event(
            "model-sync",
            AgentEvent::SessionStarted {
                provider_session_id: "provider-session".into(),
                resume: agent::ResumeCursor(serde_json::json!({
                    "session_id": "provider-session"
                })),
                model: Some("claude-sonnet-4-6".into()),
            },
            cx,
        );
        let active = state.selected_session().unwrap();
        assert_eq!(active.meta.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(active.live_model, active.meta.model);
        assert!(!active.model_changed_while_live());

        state.send_turn("model-sync", "first message".into(), Vec::new(), cx);
        assert!(matches!(
            actor.try_recv(),
            Ok(SessionCommand::SendTurn { .. })
        ));
        assert!(actor.try_recv().is_err(), "phantom restart sent Shutdown");
    });
}
#[test]
fn parking_preserves_live_providers_and_only_starts_grace_when_idle() {
    for background_tasks in [0, 2] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-parking-liveness");
        let state = cx.new_entity(TestClientState::new((*store).clone()));
        let (commands, actor) = smol::channel::unbounded();
        let mut session = live_session(ProviderKind::ClaudeCode, commands);
        session.meta.id = "resident".into();
        session.background_task_count = background_tasks;
        state.update(cx, |state, cx| {
            state.install_selected(session);
            state.park_active(cx);
            assert!(state.selected_session().is_none());
            let parked = &state.residents.parked["resident"];
            assert!(matches!(parked.runtime, Runtime::Live(_)));
            assert_eq!(parked.background_task_count, background_tasks);
            assert_eq!(parked.idle_since.is_some(), background_tasks == 0);
            assert!(actor.try_recv().is_err(), "parking sent Shutdown");
        });
    }
}

#[test]
fn select_session_readopts_idle_resident_without_changing_recency_or_shutdown() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-readopt-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "idle-resident".into();
    let updated_at = now_secs() - 3600;
    session.meta.updated_at = updated_at;
    let meta = session.meta.clone();
    test_store.upsert_meta(&meta).unwrap();

    state.update(cx, |state, cx| {
        state.sessions.push(meta);
        state.install_selected(session);
        state.park_active(cx);
        state.select_session("idle-resident", cx);

        let active = state.selected_session().unwrap();
        assert_eq!(active.meta.id, "idle-resident");
        assert_eq!(active.meta.updated_at, updated_at);
        assert_eq!(state.sessions[0].updated_at, updated_at);
        assert!(matches!(active.runtime, Runtime::Live(_)));
        assert!(!state.residents.parked.contains_key("idle-resident"));
        assert!(actor.try_recv().is_err(), "re-adoption sent Shutdown");
    });
    cx.run_until_parked();
    assert_eq!(test_store.load_index().unwrap()[0].updated_at, updated_at);
}

#[test]
fn subscribing_readopts_an_uncommitted_draft_before_its_idle_reaper() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-draft-subscription-readopt-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    state.update(cx, |state, cx| {
        let id = AppState::start_draft(state, "project".into(), PathBuf::from("/tmp"), cx);
        let subscription = tcode_protocol::Subscription {
            known_index: None,
            topic: Topic::SessionStatus {
                session_id: id.clone(),
            },
            after: None,
        };
        state.subscribe(&subscription, cx);
        state.unsubscribe(&subscription, cx);
        assert!(state.residents.parked.contains_key(&id));
        assert!(state.sessions.iter().all(|meta| meta.id != id));
        let idle_since = state.residents.parked[&id].idle_since.unwrap();
        state.subscribe(&subscription, cx);
        state.reap_idle_resident(&id, idle_since, cx);
        assert!(state.residents.live[&id].draft);
        assert!(!state.residents.parked.contains_key(&id));
        assert_eq!(state.residents.live.len(), 1);
    });
}

#[test]
fn resident_idle_reaper_shuts_down_untouched_provider() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-reaper-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "idle-resident".into();

    state.update(cx, |state, cx| {
        state.resident_idle_grace = Duration::from_millis(1);
        state.install_selected(session);
        state.park_active(cx);
    });
    // The grace timer runs on the background executor, so wait for its
    // completion to land rather than for the mailbox to look idle.
    cx.run_until(|state| !state.residents.parked.contains_key("idle-resident"));

    assert!(matches!(actor.try_recv(), Ok(SessionCommand::Shutdown)));
}

#[test]
fn idle_grace_runs_from_the_last_turn_end_whether_viewed_or_parked() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-grace-turn-end-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "idle-resident".into();
    let meta = session.meta.clone();
    let complete_turn = |state: &mut AppState, turn: &str, cx: &mut HostCx| {
        state.on_event(
            "idle-resident",
            AgentEvent::TurnStarted {
                turn_id: turn.into(),
            },
            cx,
        );
        state.on_event(
            "idle-resident",
            AgentEvent::TurnCompleted {
                turn_id: turn.into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        state.residents.live["idle-resident"].idle_since.unwrap()
    };

    state.update(cx, |state, cx| {
        state.sessions.push(meta);
        state.install_selected(session);
        let first_turn_end = complete_turn(state, "first", cx);
        state.park_active(cx);
        assert_eq!(
            state.residents.parked["idle-resident"].idle_since,
            Some(first_turn_end),
            "parking restarted the grace"
        );
        state.select_session("idle-resident", cx);
        let second_turn_end = complete_turn(state, "second", cx);
        // Fire both grace timers now; the real ones would land identically
        // but later. The first idle period ended with the second turn.
        state.reap_idle_resident("idle-resident", first_turn_end, cx);
        assert!(next_command(&actor).is_err(), "stale timer sent Shutdown");
        state.reap_idle_resident("idle-resident", second_turn_end, cx);

        let active = state.selected_session().unwrap();
        assert_eq!(active.meta.id, "idle-resident");
        assert!(matches!(active.runtime, Runtime::Idle));
        assert!(matches!(next_command(&actor), Ok(SessionCommand::Shutdown)));
    });
}

#[test]
fn resident_idle_lru_evicts_only_oldest_provider() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-lru-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let base = Instant::now();
    let mut actors = Vec::new();

    state.update(cx, |state, cx| {
        for index in 0..MAX_IDLE_RESIDENTS {
            let (commands, actor) = smol::channel::unbounded();
            let mut resident = live_session(ProviderKind::ClaudeCode, commands);
            resident.meta.id = format!("resident-{index}");
            resident.idle_since =
                Some(base - Duration::from_secs((MAX_IDLE_RESIDENTS - index) as u64));
            state
                .residents
                .parked
                .insert(resident.meta.id.clone(), resident);
            actors.push(actor);
        }

        let (commands, newest_actor) = smol::channel::unbounded();
        let mut newest = live_session(ProviderKind::ClaudeCode, commands);
        newest.meta.id = "resident-newest".into();
        state.install_selected(newest);
        state.park_active(cx);
        actors.push(newest_actor);

        assert!(!state.residents.parked.contains_key("resident-0"));
        assert_eq!(state.residents.parked.len(), MAX_IDLE_RESIDENTS);
    });

    assert!(matches!(actors[0].try_recv(), Ok(SessionCommand::Shutdown)));
    for actor in &actors[1..] {
        assert!(actor.try_recv().is_err(), "non-LRU resident was shut down");
    }
}

/// A restart would kill a background task, so it waits for the task; a message
/// does not. It goes out on the running process, says it kept the old settings,
/// and the message after the work ends takes the restart.
#[test]
fn settings_restart_waits_for_background_follow_up() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-background-restart-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "background-restart".into();
    session.live_model = Some("claude-opus-4-8".into());
    session.meta.model = Some("claude-sonnet-4-6".into());
    session.background_task_count = 1;

    state.update(cx, |state, _| {
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .binary_path = Some("/nonexistent/tcode-test-claude".into());
        state.sessions.push(session.meta.clone());
        state.install_selected(session);
    });
    let send = |state: &TestEntity, cx: &mut TestAppContext, id, text: &str| {
        state.dispatch_command(
            cx,
            id,
            Command::SendTurn {
                session_id: "background-restart".into(),
                text: text.into(),
                attachment_paths: Vec::new(),
            },
        );
    };

    send(&state, cx, 1, "goes out beside the task");
    assert!(matches!(
        next_command(&actor),
        Ok(SessionCommand::ApplySettings { .. })
    ));
    state.update(cx, |state, cx| {
        state.on_event(
            "background-restart",
            AgentEvent::SettingsApplied { applied: false },
            cx,
        );
    });
    let delivery_id = match next_command(&actor) {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) => {
            assert_eq!(text, "goes out beside the task");
            delivery_id
        }
        other => panic!("the message must go out on the running process: {other:?}"),
    };
    state.update(cx, |state, cx| {
        state.on_event(
            "background-restart",
            AgentEvent::TurnAccepted { delivery_id },
            cx,
        );
        let session = state.selected_session().unwrap();
        assert!(session.queue.is_empty());
        let entries = &session.timeline.entries;
        assert!(
            matches!(entries.last().unwrap().content, EntryContent::SettingsKept),
            "the message says it kept the old settings: {:?}",
            entries
                .iter()
                .map(|entry| &entry.content)
                .collect::<Vec<_>>()
        );
        state.on_event(
            "background-restart",
            AgentEvent::TurnCompleted {
                turn_id: "beside-the-task".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
    });
    assert!(
        next_command(&actor).is_err(),
        "the running task keeps its process"
    );

    // An early drain still cannot restart the provider: its self-invoked
    // follow-up turn may be about to start.
    state.update(cx, |state, cx| {
        state.on_event(
            "background-restart",
            AgentEvent::BackgroundTasksChanged { count: 0 },
            cx,
        );
    });
    assert!(next_command(&actor).is_err());

    send(&state, cx, 2, "takes the new model");
    assert!(matches!(next_command(&actor), Ok(SessionCommand::Shutdown)));
    state.read(|state| assert_eq!(state.selected_session().unwrap().queue.len(), 1));
}

/// A harness that can take a new model and effort where it runs is asked to,
/// and the message that asked goes out behind its answer, on the same process,
/// background work or none.
#[test]
fn a_new_model_is_taken_on_the_running_process() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-live-settings-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "live-settings".into();
    session.live_model = Some("claude-opus-4-8".into());
    session.meta.model = Some("claude-sonnet-4-6".into());
    session.background_task_count = 1;
    let effort = OptionSelection {
        id: "reasoningEffort".into(),
        value: serde_json::json!("low"),
    };
    session.meta.option_selections.push(effort.clone());

    state.update(cx, |state, _| {
        state.sessions.push(session.meta.clone());
        state.install_selected(session);
    });
    state.dispatch_command(
        cx,
        1,
        Command::SendTurn {
            session_id: "live-settings".into(),
            text: "on the new model".into(),
            attachment_paths: Vec::new(),
        },
    );
    match next_command(&actor) {
        Ok(SessionCommand::ApplySettings {
            model,
            option_selections,
        }) => {
            assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
            assert_eq!(option_selections, [effort]);
        }
        other => panic!("expected the settings to go first: {other:?}"),
    }
    assert!(
        next_command(&actor).is_err(),
        "the message waits its answer"
    );

    state.update(cx, |state, cx| {
        state.on_event(
            "live-settings",
            AgentEvent::SettingsApplied { applied: true },
            cx,
        );
    });
    let delivery_id = match next_command(&actor) {
        Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
        other => panic!("the message goes out behind the answer: {other:?}"),
    };
    state.update(cx, |state, cx| {
        state.on_event(
            "live-settings",
            AgentEvent::TurnAccepted { delivery_id },
            cx,
        );
        let session = state.selected_session().unwrap();
        assert!(
            !session
                .timeline
                .entries
                .iter()
                .any(|entry| matches!(entry.content, EntryContent::SettingsKept)),
            "the message ran on the settings it asked for"
        );
        assert!(!session.launch_settings_changed_while_live());
    });
}

/// What the process refuses takes a restart: at once when nothing would be
/// killed, and otherwise after the background work, with the message going out
/// on the old settings in the meantime.
#[test]
fn a_refused_settings_change_falls_back_to_the_restart() {
    for background in [0, 1] {
        let cx = &mut TestAppContext::default();
        let test_store = TestStore::new("tcode-refused-settings-test");
        let state = cx.new_entity(TestClientState::new((*test_store).clone()));
        let (commands, actor) = smol::channel::unbounded();
        let mut session = live_session(ProviderKind::ClaudeCode, commands);
        session.meta.id = "refused-settings".into();
        session.live_model = Some("claude-opus-4-8".into());
        session.meta.model = Some("claude-sonnet-4-6".into());
        session.background_task_count = background;

        state.update(cx, |state, cx| {
            state
                .settings
                .provider_mut(ProviderKind::ClaudeCode)
                .binary_path = Some("/nonexistent/tcode-test-claude".into());
            state.sessions.push(session.meta.clone());
            state.install_selected(session);
            state.send_turn("refused-settings", "go".into(), Vec::new(), cx);
            assert!(matches!(
                next_command(&actor),
                Ok(SessionCommand::ApplySettings { .. })
            ));
            state.on_event(
                "refused-settings",
                AgentEvent::SettingsApplied { applied: false },
                cx,
            );
        });
        match (background, next_command(&actor)) {
            (0, Ok(SessionCommand::Shutdown)) | (1, Ok(SessionCommand::SendTurn { .. })) => {}
            other => panic!("with {background} background task(s): {other:?}"),
        }
    }
}

/// Whether a lead's agents hold it: one still runs, or a dispatched one
/// finished and awaits its settle.
fn children_hold(activity: &tcode_protocol::SessionActivity) -> bool {
    activity.background.agents > 0 || activity.unsettled > 0
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
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, actor) = smol::channel::unbounded();
    let task = agent::BackgroundTask {
        id: "bl5ti808a".into(),
        name: "sleep 300".into(),
        kind: agent::BackgroundKind::Command,
        item_id: Some("toolu-sleep".into()),
        started_at: 1,
        started_reported: false,
    };
    let listed = |state: &TestEntity| {
        state.read(|state| {
            state
                .session_status_snapshot("lists")
                .unwrap()
                .background_tasks
        })
    };
    state.update(cx, |state, cx| {
        let mut session = live_session(ProviderKind::ClaudeCode, commands.clone());
        session.meta.id = "lists".into();
        state.sessions.push(session.meta.clone());
        state.install_selected(session);
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
        for event in [
            AgentEvent::TurnStarted {
                turn_id: "t1".into(),
            },
            started(
                "toolu-sleep",
                ItemContent::CommandExecution {
                    command: "sleep 300".into(),
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
                    output: String::new(),
                    exit_code: None,
                    status: ItemStatus::InProgress,
                },
            }),
            AgentEvent::BackgroundTaskList {
                tasks: vec![task.clone()],
            },
        ] {
            state.on_event("lists", event, cx);
        }
    });
    // The subagent runs in the background, and counts there alone.
    assert_eq!(
        state.read(|state| state.index_snapshot().summary.activity["lists"].background),
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
    let now = listed(&state);
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
    state.update(cx, |state, _| {
        let mut session = live_session(ProviderKind::Pi, commands);
        session.meta.id = "cannot".into();
        state.sessions.push(session.meta.clone());
        state
            .residents
            .live
            .insert(session.meta.id.clone(), session);
    });

    for (id, session_id, task_id) in [
        (1, "lists", Some(task.id.clone())),
        (2, "lists", None),
        (3, "cannot", None),
    ] {
        state.dispatch_command(
            cx,
            id,
            Command::StopBackgroundTask {
                session_id: session_id.into(),
                task_id,
            },
        );
    }
    for (id, session_id) in [(4, "lists"), (5, "cannot")] {
        state.dispatch_command(
            cx,
            id,
            Command::MoveToBackground {
                session_id: session_id.into(),
                item_id: "toolu-build".into(),
            },
        );
    }
    assert!(matches!(
        next_command(&actor),
        Ok(SessionCommand::StopBackgroundTask { task_id: Some(id) }) if id == task.id
    ));
    assert!(matches!(
        next_command(&actor),
        Ok(SessionCommand::StopBackgroundTask { task_id: None })
    ));
    assert!(matches!(
        next_command(&actor),
        Ok(SessionCommand::MoveToBackground { item_id }) if item_id == "toolu-build"
    ));
    assert!(next_command(&actor).is_err());
    assert!(
        listed(&state)[0].stopping_since.is_some(),
        "the stop shows until the harness drops the task"
    );

    state.update(cx, |state, cx| {
        state.on_event(
            "lists",
            AgentEvent::BackgroundTaskList { tasks: Vec::new() },
            cx,
        )
    });
    let now = listed(&state);
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
/// interrupted, the thread stops stopping, and the next message starts it again.
#[test]
fn force_terminate_kills_a_harness_that_does_not_answer_a_stop() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-force-terminate-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "unanswering".into();
    state.update(cx, |state, cx| {
        state.sessions.push(session.meta.clone());
        state.install_selected(session);
        state.on_event(
            "unanswering",
            AgentEvent::TurnStarted {
                turn_id: "hung".into(),
            },
            cx,
        );
    });
    state.dispatch_command(
        cx,
        1,
        Command::Interrupt {
            session_id: "unanswering".into(),
        },
    );
    assert!(matches!(
        next_command(&actor),
        Ok(SessionCommand::Interrupt)
    ));
    state.read(|state| {
        let status = state.session_status_snapshot("unanswering").unwrap();
        assert!(status.stopping && status.stopping_since.is_some());
    });

    state.dispatch_command(
        cx,
        2,
        Command::ForceTerminate {
            session_id: "unanswering".into(),
        },
    );
    assert!(matches!(next_command(&actor), Ok(SessionCommand::Shutdown)));
    state.read(|state| {
        let session = state.selected_session().unwrap();
        assert!(matches!(session.runtime, Runtime::Idle));
        assert_eq!(
            session.timeline.turns.last().unwrap().status,
            Some(TurnStatus::Interrupted)
        );
        let status = state.session_status_snapshot("unanswering").unwrap();
        assert!(!status.stopping && !status.activity.turn_running);
    });
}

/// Background work takes no interrupt: Claude Code acknowledges one and the
/// task runs on, so a thread with nothing else running must not read as stopping.
#[test]
fn an_interrupt_leaves_a_thread_with_only_background_work_alone() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-background-interrupt-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "only-background".into();
    session.background_task_count = 1;
    state.update(cx, |state, _| {
        state.sessions.push(session.meta.clone());
        state.install_selected(session);
    });
    state.dispatch_command(
        cx,
        1,
        Command::Interrupt {
            session_id: "only-background".into(),
        },
    );
    assert!(next_command(&actor).is_err());
    state.read(|state| assert!(!state.selected_session().unwrap().interrupt_requested));
}

/// A report arriving while a restart waits on background work, with no turn to
/// steer into, wakes the lead on its running process.
#[test]
fn a_report_reaches_a_lead_whose_restart_waits_for_background_work() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-background-report-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, actor) = smol::channel::unbounded();
    let mut lead = live_session(ProviderKind::ClaudeCode, commands);
    lead.meta.id = "lead".into();
    lead.live_model = Some("claude-opus-4-8".into());
    lead.meta.model = Some("claude-sonnet-4-6".into());
    lead.background_task_count = 1;

    state.update(cx, |state, cx| {
        state.sessions.push(lead.meta.clone());
        state.install_selected(lead);
        state.deliver_orchestrate_callback_to_parent("lead", "the report".into(), cx);
    });
    assert!(matches!(
        next_command(&actor),
        Ok(SessionCommand::ApplySettings { .. })
    ));
    state.update(cx, |state, cx| {
        state.on_event("lead", AgentEvent::SettingsApplied { applied: false }, cx);
    });
    assert!(matches!(
        next_command(&actor),
        Ok(SessionCommand::SendTurn { text, .. }) if text.contains("the report")
    ));
}

/// A lead's queue does not outlive the host: a report waiting there when the
/// host stops goes to the lead when it starts again, and only until the lead's
/// provider takes it.
#[test]
fn a_report_queued_when_the_host_stops_reaches_the_lead_once_after_it_starts() {
    let test_store = TestStore::new("tcode-report-restart-test");
    let mut lead = SessionMeta::new(ProviderKind::Acp, test_store.root().clone(), None);
    lead.id = "lead".into();
    let mut child = SessionMeta::new(ProviderKind::Codex, test_store.root().clone(), None);
    child.id = "child".into();
    child.parent_session_id = Some("lead".into());
    child.expectation = Some(ChildExpectation::opened(1));
    test_store.upsert_meta(&lead).unwrap();
    test_store.upsert_meta(&child).unwrap();

    let start = |lead_busy: bool| {
        let mut cx = TestAppContext::default();
        let state = cx.new_entity(TestClientState::new((*test_store).clone()));
        let (commands, actor) = smol::channel::unbounded();
        state.update(&mut cx, |state, cx| {
            let meta = state.find_meta("lead").unwrap();
            let mut session = ActiveSession {
                runtime: Runtime::Live(commands),
                ..ActiveSession::new(meta, false, Vec::new())
            };
            session.turn_in_flight = lead_busy;
            state.install_selected(session);
            state.resume_undelivered_reports(cx);
        });
        (cx, state, actor)
    };

    // The lead's turn runs on a harness that cannot steer, so the report waits.
    let (mut cx, state, actor) = start(true);
    state.update(&mut cx, |state, cx| {
        state.fail_child_expectation("child", "the provider closed", cx);
        assert_eq!(state.selected_session().unwrap().queue.len(), 1);
    });
    cx.run_until_parked();
    assert!(next_command(&actor).is_err());
    drop((cx, state));

    let (mut cx, state, actor) = start(false);
    let delivery_id = match next_command(&actor) {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) => {
            assert!(text.contains("the provider closed"), "{text}");
            delivery_id
        }
        other => panic!("the report goes out after the restart: {other:?}"),
    };
    state.update(&mut cx, |state, cx| {
        state.on_event("lead", AgentEvent::TurnAccepted { delivery_id }, cx);
    });
    cx.run_until_parked();
    drop((cx, state));

    let (_cx, _state, actor) = start(false);
    assert!(next_command(&actor).is_err(), "taken once, sent once");
}

/// Work a thread left running in the background ends as interrupted when
/// the thread is reopened after the host restarted, since no process can
/// finish it; a thread whose process still runs keeps it open, and so does
/// a native subagent's mirror whose parent's process does.
#[test]
fn background_work_no_process_can_finish_ends_as_interrupted_on_load() {
    fn backgrounded(state: &mut AppState, id: &str, cx: &mut HostCx) {
        for event in [
            AgentEvent::TurnStarted {
                turn_id: "turn".into(),
            },
            AgentEvent::ItemStarted(ThreadItem {
                id: "serve".into(),
                parent_item_id: None,
                content: ItemContent::CommandExecution {
                    command: "serve".into(),
                    output: String::new(),
                    exit_code: None,
                    status: ItemStatus::InProgress,
                },
            }),
            AgentEvent::ItemBackgrounded {
                item_id: "serve".into(),
                parent_item_id: None,
            },
            AgentEvent::TurnCompleted {
                turn_id: "turn".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ] {
            state.record_event(id, &event, cx);
        }
    }
    fn status(state: &AppState, id: &str) -> ItemStatus {
        state
            .resident(id)
            .unwrap()
            .timeline
            .entries
            .iter()
            .find_map(|entry| match &entry.content {
                tcode_core::session::EntryContent::Item(ItemContent::CommandExecution {
                    status,
                    ..
                }) => Some(*status),
                _ => None,
            })
            .unwrap()
    }
    let test_store = TestStore::new("tcode-unowned-background-test");
    let (commands, _actor) = smol::channel::unbounded();
    let mut live = live_session(ProviderKind::ClaudeCode, commands);
    live.meta.id = "live".into();
    let mut cold = SessionMeta::new(ProviderKind::ClaudeCode, test_store.root().clone(), None);
    cold.id = "cold".into();
    {
        let mut cx = TestAppContext::default();
        let state = cx.new_entity(TestClientState::new((*test_store).clone()));
        state.update(&mut cx, |state, cx| {
            state.sessions.push(cold.clone());
            state.persist_meta(&cold, cx);
            let mut process = live_session(ProviderKind::ClaudeCode, smol::channel::unbounded().0);
            process.meta = cold.clone();
            state.residents.live.insert(cold.id.clone(), process);
            backgrounded(state, "cold", cx);
        });
        cx.run_until_parked();
    }

    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    state.update(&mut cx, |state, cx| {
        let mut mirror =
            SessionMeta::new(ProviderKind::ClaudeCode, test_store.root().clone(), None);
        mirror.id = "mirror".into();
        mirror.parent_session_id = Some("live".into());
        mirror.native_subagent = Some("task".into());
        state.sessions.push(mirror.clone());
        state.persist_meta(&mirror, cx);
        state.residents.parked.insert(
            mirror.id.clone(),
            ActiveSession::new(mirror, false, Vec::new()),
        );
        backgrounded(state, "mirror", cx);
        state.sessions.push(live.meta.clone());
        state.install_selected(live);
        backgrounded(state, "live", cx);
        state.park_active(cx);
        state.select_session("cold", cx);
    });
    // The reopened thread's log is read and folded off the host.
    cx.run_until(|state| {
        state
            .resident("cold")
            .is_some_and(|cold| !cold.timeline.entries.is_empty())
    });
    state.read(|state| assert_eq!(status(state, "cold"), ItemStatus::Interrupted));
    state.reopen(&mut cx, "live");
    state.read(|state| assert_eq!(status(state, "live"), ItemStatus::InProgress));
    state.reopen(&mut cx, "mirror");
    state.read(|state| assert_eq!(status(state, "mirror"), ItemStatus::InProgress));
    let log = test_store.read_events("cold").unwrap();
    assert!(matches!(
        &log.last().unwrap().event,
        AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::CommandExecution {
                status: ItemStatus::Interrupted,
                ..
            },
            ..
        })
    ));
}

/// The record that the lead's provider took a child's report and the child no
/// longer owing it are saved together: a host that stops right after saving
/// that record, as a turn or as a steer, does not send the report again.
#[test]
fn a_report_the_lead_took_is_saved_as_taken_with_the_record_of_it() {
    for steered in [false, true] {
        let test_store = TestStore::new("tcode-report-taken-with-record-test");
        let mut child = SessionMeta::new(ProviderKind::Codex, test_store.root().clone(), None);
        child.id = "child".into();
        child.parent_session_id = Some("lead".into());
        child.expectation = Some(ChildExpectation::opened(1));
        let (commands, actor) = smol::channel::unbounded();
        let mut lead = live_session(ProviderKind::ClaudeCode, commands);
        lead.meta.id = "lead".into();
        lead.turn_in_flight = steered;
        test_store.upsert_meta(&lead.meta).unwrap();
        test_store.upsert_meta(&child).unwrap();

        let mut cx = TestAppContext::default();
        let state = cx.new_entity(TestClientState::new((*test_store).clone()));
        let taken = state.update(&mut cx, |state, cx| {
            state.sessions.push(lead.meta.clone());
            state.install_selected(lead);
            state.fail_child_expectation("child", "the provider closed", cx);
            match next_command(&actor) {
                Ok(SessionCommand::Steer { request_id, .. }) if steered => {
                    AgentEvent::SteerAccepted { request_id }
                }
                Ok(SessionCommand::SendTurn { delivery_id, .. }) if !steered => {
                    AgentEvent::TurnAccepted { delivery_id }
                }
                other => panic!("the report goes to the lead: {other:?}"),
            }
        });
        cx.run_until_parked();

        // The host stops once the record of the lead taking the report is
        // saved, before anything written after it.
        let (writes, held) = smol::channel::unbounded();
        state.update(&mut cx, |state, cx| {
            drop(std::mem::replace(&mut state.store_writes, writes));
            state.store_writer.take().unwrap().join().unwrap();
            state.on_event("lead", taken, cx);
        });
        let held: Vec<_> = std::iter::from_fn(|| held.try_recv().ok()).collect();
        let record = held
            .iter()
            .position(|write| {
                matches!(write, StoreWrite::AppendEvent { id, event, .. } if id == "lead"
                && matches!(**event, AgentEvent::SteerAccepted { .. }
                    | AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::UserMessage { .. },
                        ..
                    })))
            })
            .expect("the lead's log records it");
        let (failures, _) = smol::channel::unbounded();
        let writer = StoreWriter::new(
            (*test_store).clone(),
            SettingsStore::new(test_store.root().clone()),
            failures,
        );
        let (saved, queue) = smol::channel::unbounded();
        for write in held.into_iter().take(record + 1) {
            saved.try_send(write).unwrap();
        }
        drop(saved);
        writer.run(queue);
        drop((cx, state));

        let mut cx = TestAppContext::default();
        let state = cx.new_entity(TestClientState::new((*test_store).clone()));
        state.update(&mut cx, |state, _| {
            let expectation = state.find_meta("child").unwrap().expectation.unwrap();
            assert_eq!(expectation.undelivered, None, "steered: {steered}");
        });
    }
}

/// A provider that refuses a steer because the turn just ended has not seen it:
/// a child's report steered into that turn goes out as a turn of its own.
#[test]
fn a_rejected_steer_of_a_report_goes_out_as_a_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-rejected-report-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, actor) = smol::channel::unbounded();
    let mut lead = live_session(ProviderKind::Codex, commands);
    lead.meta.id = "lead".into();

    state.update(cx, |state, cx| {
        state.sessions.push(lead.meta.clone());
        state.install_selected(lead);
        state.on_event(
            "lead",
            AgentEvent::TurnStarted {
                turn_id: "ending".into(),
            },
            cx,
        );
        state.deliver_orchestrate_callback_to_parent("lead", "the report".into(), cx);
        let Ok(SessionCommand::Steer { request_id, .. }) = next_command(&actor) else {
            panic!("a running turn takes the report as a steer");
        };
        state.on_event(
            "lead",
            AgentEvent::TurnCompleted {
                turn_id: "ending".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        state.on_event("lead", AgentEvent::SteerRejected { request_id }, cx);
    });
    assert!(matches!(
        next_command(&actor),
        Ok(SessionCommand::SendTurn { text, .. }) if text == "the report"
    ));
}

/// A steer the provider never confirmed died with its process, so a restart
/// sends it to the next one.
#[test]
fn a_steer_lost_with_its_process_goes_to_the_next_one() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-lost-steer-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, actor) = smol::channel::unbounded();
    let mut lead = live_session(ProviderKind::ClaudeCode, commands);
    lead.meta.id = "lead".into();

    state.update(cx, |state, cx| {
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .binary_path = Some("/nonexistent/tcode-test-claude".into());
        state.sessions.push(lead.meta.clone());
        state.install_selected(lead);
        state.on_event(
            "lead",
            AgentEvent::TurnStarted {
                turn_id: "running".into(),
            },
            cx,
        );
        state.deliver_orchestrate_callback_to_parent("lead", "the report".into(), cx);
        assert!(matches!(
            next_command(&actor),
            Ok(SessionCommand::Steer { .. })
        ));
        state.on_event(
            "lead",
            AgentEvent::SessionClosed {
                reason: Some("claude exited".into()),
            },
            cx,
        );
        let session = state.selected_session().unwrap();
        assert_eq!(session.queue.len(), 1);
        assert_eq!(session.queue[0].text, "the report");
        assert_eq!(session.queue[0].origin, MessageOrigin::Agent);
    });
}

/// A message sent with a new model while a turn runs (a `/compact` included)
/// waits for that turn instead of killing it to restart the provider.
#[test]
fn settings_restart_waits_for_the_running_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-running-turn-restart-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::Grok, commands);
    session.meta.id = "running-turn-restart".into();
    session.live_model = Some("grok-4".into());
    session.meta.model = Some("grok-5".into());

    state.update(cx, |state, cx| {
        state.settings.provider_mut(ProviderKind::Grok).binary_path =
            Some("/nonexistent/tcode-test-grok".into());
        state.install_selected(session);
        state.on_event(
            "running-turn-restart",
            AgentEvent::TurnStarted {
                turn_id: "compact".into(),
            },
            cx,
        );
        state.send_turn(
            "running-turn-restart",
            "continue on the new model".into(),
            Vec::new(),
            cx,
        );
        assert!(next_command(&actor).is_err());
        assert_eq!(state.selected_session().unwrap().queue.len(), 1);

        state.on_event(
            "running-turn-restart",
            AgentEvent::TurnCompleted {
                turn_id: "compact".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(matches!(next_command(&actor), Ok(SessionCommand::Shutdown)));
        assert_eq!(state.selected_session().unwrap().queue.len(), 1);
    });
}

#[test]
fn fork_thread_clones_timeline_and_provider_cursor() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-fork-test");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    let mut source = SessionMeta::new(
        ProviderKind::Codex,
        PathBuf::from("/tmp/source-worktree"),
        Some("gpt-5.4".into()),
    );
    source.title = "Investigate parser".into();
    source.resume_cursor = Some(agent::ResumeCursor(
        serde_json::json!({"thread_id": "native-source"}),
    ));
    source.worktree = Some(WorktreeInfo {
        root_project_path: PathBuf::from("/tmp/project"),
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
    store.upsert_meta(&source).unwrap();
    store
        .append_event(
            &source.id,
            1,
            &AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
        )
        .unwrap();
    let mut empty_source = SessionMeta::new(
        ProviderKind::Codex,
        root.join("empty-source"),
        Some("gpt-5.4".into()),
    );
    empty_source.title = "Native conversation without local events".into();
    empty_source.resume_cursor = Some(agent::ResumeCursor(
        serde_json::json!({"thread_id": "native-empty-source"}),
    ));
    store.upsert_meta(&empty_source).unwrap();
    assert!(store.read_event_log(&empty_source.id).unwrap().is_empty());
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        assert_eq!(
            state.index_snapshot().summary.activity[&source.id].fork,
            ForkAvailability::Available
        );
        let mut resident = ActiveSession::new(source.clone(), false, Vec::new());
        resident.turn_in_flight = true;
        state.install_selected(resident);
        assert_eq!(
            state.index_snapshot().summary.activity[&source.id].fork,
            ForkAvailability::Running
        );
        assert!(state.host.fork_thread(&source.id, cx).is_none());
        state.selected_session_mut().unwrap().turn_in_flight = false;
        state.selected_session_mut().unwrap().background_task_count = 1;
        assert_eq!(
            state.index_snapshot().summary.activity[&source.id].fork,
            ForkAvailability::Available
        );
    });
    state.update(cx, |state, cx| state.fork_thread(&source.id, cx));
    cx.run_until(|state| {
        state
            .selected_session()
            .is_some_and(|session| session.timeline.turns.len() == 1)
    });

    state.update(cx, |state, _cx| {
        let active = state.selected_session().unwrap();
        let fork = &active.meta;
        assert_ne!(fork.id, source.id);
        assert_eq!(fork.pending_fork, Some(agent::ForkPoint::Tail));
        assert!(fork.pull_requests.is_empty());
        assert_eq!(
            fork.resume_cursor.as_ref().unwrap().0["thread_id"],
            "native-source"
        );
        assert_eq!(fork.cwd, source.cwd);
        assert_eq!(fork.worktree, None);
        assert!(
            state
                .index_snapshot()
                .summary
                .worktree_shared
                .contains(&source.id)
        );
        assert!(!active.timeline.turn_running);
        // The source's log, then the restore points' end in the new session,
        // and the end of the turn the source left open, which no process of
        // the fork can finish.
        let events = state.store.read_events(&fork.id).unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[1].event, AgentEvent::CheckpointsCleared));
        assert!(matches!(
            events[2].event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        ));
        assert!(
            state
                .store
                .read_event_log(&fork.id)
                .unwrap()
                .starts_with(&state.store.read_event_log(&source.id).unwrap())
        );
    });

    state.update(cx, |state, cx| state.fork_thread(&empty_source.id, cx));
    cx.run_until(|state| {
        state
            .selected_session()
            .is_some_and(|session| session.meta.resume_cursor == empty_source.resume_cursor)
    });
    state.read(|state| {
        let active = state.selected_session().unwrap();
        let fork = &active.meta;
        assert_ne!(fork.id, empty_source.id);
        assert_eq!(fork.pending_fork, Some(agent::ForkPoint::Tail));
        assert_eq!(
            fork.title,
            "Native conversation without local events (fork)"
        );
        assert_eq!(fork.cwd, empty_source.cwd);
        assert_eq!(fork.model, empty_source.model);
        assert_eq!(fork.resume_cursor, empty_source.resume_cursor);
        assert!(active.timeline.turns.is_empty());
        assert!(
            state
                .store
                .load_index()
                .unwrap()
                .iter()
                .any(|meta| meta.id == fork.id)
        );
        assert!(
            state
                .store
                .read_event_log(&empty_source.id)
                .unwrap()
                .is_empty()
        );
        // Nothing local was copied: only the end of the parent's restore points.
        assert!(matches!(
            state.store.read_events(&fork.id).unwrap().as_slice(),
            [record] if matches!(record.event, AgentEvent::CheckpointsCleared)
        ));
    });
}

#[test]
fn store_writer_flush_persists_ordered_events_metadata_and_secrets_for_reopening() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-writer-reopen");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/upsert"), None);
    meta.title = "persisted by writer".into();
    let id = meta.id.clone();
    state.update(cx, |state, cx| {
        state.persist_meta(&meta, cx);
        state.record_event(&id, &persisted_assistant_event("first"), cx);
        state.set_profile_secret("profile", "ANTHROPIC_API_KEY", Some("writer-secret"), cx);
        state.record_event(&id, &persisted_assistant_event("second"), cx);
        // More than one batch's worth, so the flush waits for several commits.
        for index in 0..600 {
            state.record_event(
                &id,
                &persisted_assistant_event(&format!("bulk {index}")),
                cx,
            );
        }
    });
    cx.run_until_parked();

    let fresh = SessionStore::open_at(test_store.root().clone()).unwrap();
    assert_eq!(
        fresh
            .load_index()
            .unwrap()
            .iter()
            .find(|meta| meta.id == id)
            .unwrap()
            .title,
        "persisted by writer"
    );
    let events = fresh.read_events(&id).unwrap();
    let texts: Vec<_> = events
        .iter()
        .filter_map(|stored| match &stored.event {
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text, .. },
                ..
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let bulk: Vec<_> = (0..600).map(|index| format!("bulk {index}")).collect();
    assert_eq!(texts[..2], ["first", "second"]);
    assert_eq!(texts[2..], bulk);
    let settings = SettingsStore::new(test_store.root().clone());
    assert_eq!(
        settings
            .profile_secrets("profile")
            .get("ANTHROPIC_API_KEY")
            .map(String::as_str),
        Some("writer-secret")
    );
}

/// Deleting a project kills the PTY its unsent draft left running off screen
/// instead of leaving it behind forever.
#[test]
fn deleting_a_project_clears_its_drafts_terminal_state() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-delete-project-draft");
    let root = test_store.root().clone();
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    state.update(cx, |state, cx| {
        state.start_draft("doomed".into(), root.clone(), cx);
    });
    let draft_id = state.read(|state| state.selected.clone().expect("draft selected"));
    open_terminal_drawer(&state, cx, 1, &draft_id);
    state.update(cx, |state, cx| state.host.park_active(&draft_id, cx));
    let destination = ConversationDestination::ProjectDraft("doomed".into());
    state.read(|state| assert!(state.host.terminal_workspaces.contains_key(&destination)));

    state.update(cx, |state, cx| state.host.delete_project("doomed", cx));
    cx.run_until_parked();

    state.read(|state| {
        assert!(
            !state.host.terminal_workspaces.contains_key(&destination),
            "the deleted project's draft kept its terminal workspace"
        );
    });
}

/// A stored thread whose working directory exists, so a terminal can start in
/// it.
fn stored_terminal_thread(store: &SessionStore) -> String {
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let meta = SessionMeta::new(ProviderKind::Codex, cwd, None);
    store.upsert_meta(&meta).unwrap();
    meta.id
}

fn session_status_topic(session_id: &str) -> Subscription {
    Subscription {
        known_index: None,
        topic: Topic::SessionStatus {
            session_id: session_id.to_owned(),
        },
        after: None,
    }
}

/// Show the conversation on a client's screen, as client message `id`, and
/// return the status the host answered with.
fn load_conversation(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    session_id: &str,
) -> SessionStatus {
    state.deliver(
        cx,
        id,
        tcode_protocol::ClientPayload::Subscribe(session_status_topic(session_id)),
    );
    cx.run_until_sent(|message| match message {
        HostMessage::Event(EventEnvelope {
            request_id: Some(reply),
            event: ServerEvent::SessionStatusReplaced(status),
            ..
        }) if *reply == id => Some(status.as_ref().clone()),
        _ => None,
    })
    .0
}

fn leave_conversation(state: &TestEntity, cx: &mut TestAppContext, id: u64, session_id: &str) {
    state.deliver(
        cx,
        id,
        tcode_protocol::ClientPayload::Unsubscribe(session_status_topic(session_id)),
    );
    cx.run_until_parked();
}

fn open_terminal_drawer(state: &TestEntity, cx: &mut TestAppContext, id: u64, session_id: &str) {
    state.dispatch_command(
        cx,
        id,
        Command::ToggleTerminalPanel {
            session_id: session_id.to_owned(),
        },
    );
    cx.run_until(|state| {
        state
            .resident(session_id)
            .is_some_and(|thread| !thread.terminal_workspace.terminals.is_empty())
    });
}

fn assert_drawer_closed_without_terminals(state: &TestEntity, status: &SessionStatus) {
    assert!(!status.terminal_open, "the drawer is open");
    assert_eq!(status.terminal_height, 240.);
    assert!(status.terminals.is_empty());
    state.read(|state| {
        assert!(
            !state
                .pending_terminal_spawns
                .contains_key(&status.session_id),
            "a terminal is starting that nobody asked for"
        );
    });
}

/// Earlier builds reopened, with fresh shells, every drawer
/// `terminal-ui.json` marked open once its conversation loaded. Its entries
/// are ignored and the file is removed.
#[test]
fn a_conversation_loads_with_its_drawer_closed_despite_an_older_builds_terminal_file() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-legacy-terminal-ui");
    let id = stored_terminal_thread(&test_store);
    let legacy = test_store.root().join("terminal-ui.json");
    std::fs::write(
        &legacy,
        format!(
            r#"{{
  "{id}": {{ "open": true, "height": 320.0, "count": 2 }},
  "draft:project": {{ "open": false, "height": 240.0, "count": 0 }}
}}"#
        ),
    )
    .unwrap();
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    assert!(!legacy.exists(), "the older build's terminal file is kept");

    let status = load_conversation(&state, cx, 1, &id);
    assert_drawer_closed_without_terminals(&state, &status);
}

/// A conversation left with a terminal running comes back with its drawer as
/// it was: open, at its height, with the same terminals. One whose shells
/// have all exited comes back closed, without them.
#[test]
fn only_a_running_terminal_reopens_the_drawer_when_its_conversation_loads() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-terminal-restore");
    let id = stored_terminal_thread(&test_store);
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    load_conversation(&state, cx, 1, &id);
    open_terminal_drawer(&state, cx, 2, &id);
    state.dispatch_command(
        cx,
        3,
        Command::NewTerminal {
            session_id: id.clone(),
        },
    );
    cx.run_until(|state| {
        state
            .resident(&id)
            .is_some_and(|thread| thread.terminal_workspace.terminals.len() == 2)
    });
    state.dispatch_command(
        cx,
        4,
        Command::SetTerminalHeight {
            session_id: id.clone(),
            height: 320.,
        },
    );
    let left = load_conversation(&state, cx, 5, &id).terminals;
    leave_conversation(&state, cx, 6, &id);

    let status = load_conversation(&state, cx, 7, &id);
    assert!(status.terminal_open);
    assert_eq!(status.terminal_height, 320.);
    assert_eq!(
        status.terminals.iter().map(|t| t.id).collect::<Vec<_>>(),
        left.iter().map(|t| t.id).collect::<Vec<_>>()
    );

    for (request, terminal) in (8..).zip(&left) {
        state.dispatch_command(
            cx,
            request,
            Command::TerminalInput {
                terminal_id: terminal.id,
                bytes: b"exit\r".to_vec(),
            },
        );
    }
    cx.run_until(|state| {
        left.iter().all(|terminal| {
            state
                .terminal_handle(terminal.id)
                .is_some_and(|handle| handle.exited())
        })
    });
    leave_conversation(&state, cx, 20, &id);

    let status = load_conversation(&state, cx, 21, &id);
    assert_drawer_closed_without_terminals(&state, &status);
    assert!(!test_store.root().join("terminal-ui.json").exists());
}

/// Closing a conversation's last terminal forgets its drawer: it comes back
/// neither open nor at the height it had.
#[test]
fn closing_the_last_terminal_forgets_the_drawer() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-terminal-close-last");
    let id = stored_terminal_thread(&test_store);
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    load_conversation(&state, cx, 1, &id);
    open_terminal_drawer(&state, cx, 2, &id);
    state.dispatch_command(
        cx,
        3,
        Command::SetTerminalHeight {
            session_id: id.clone(),
            height: 320.,
        },
    );
    let terminal_id = load_conversation(&state, cx, 4, &id).terminals[0].id;
    state.dispatch_command(
        cx,
        5,
        Command::CloseTerminal {
            session_id: id.clone(),
            terminal_id,
        },
    );
    let status = load_conversation(&state, cx, 6, &id);
    assert_drawer_closed_without_terminals(&state, &status);

    leave_conversation(&state, cx, 7, &id);
    let status = load_conversation(&state, cx, 8, &id);
    assert_drawer_closed_without_terminals(&state, &status);
    assert!(!test_store.root().join("terminal-ui.json").exists());
}

#[test]
fn terminal_open_installs_after_executor_pump_in_the_project_cwd() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-terminal-open");
    let root = test_store.root().clone();
    let project_cwd = root.join("project");
    std::fs::create_dir_all(&project_cwd).unwrap();
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, _| {
        state.install_selected(AppState::build_draft_session(
            "terminal-project".into(),
            project_cwd.clone(),
            ProviderKind::Codex,
            None,
            None,
            Vec::new(),
        ));
    });
    state.update(cx, |state, cx| {
        state
            .host
            .open_terminal_panel(state.selected.as_deref().unwrap_or_default(), cx)
    });
    state.read(|state| {
        assert!(
            state
                .selected_session()
                .unwrap()
                .terminal_workspace
                .terminals
                .is_empty()
        );
    });

    cx.run_until(|state| {
        state
            .selected_session()
            .is_some_and(|session| !session.terminal_workspace.terminals.is_empty())
    });

    state.read(|state| {
        let workspace = &state.selected_session().unwrap().terminal_workspace;
        assert_eq!(workspace.terminals.len(), 1);
        assert!(
            state
                .host
                .terminal_panel_open(state.selected.as_deref().unwrap_or_default())
        );
        assert_eq!(workspace.terminals[0].terminal.cwd(), project_cwd);
    });
}

/// Compaction is a turn of the provider's own, as is a goal change where the
/// harness runs it as one (Claude Code's `/goal`), so each is forwarded only to
/// an idle live provider; over a running turn the command is refused.
#[test]
fn compaction_waits_for_the_running_turn() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-compact-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-compact-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (session, commands) = fake_live_session(cwd.clone());
    let id = session.meta.id.clone();

    state.update(cx, |state, cx| {
        state.store.upsert_meta(&session.meta).unwrap();
        state.sessions = state.store.load_index().unwrap().into();
        state.install_selected(session);
        state.send_turn(&id, "finish this task".into(), Vec::new(), cx);
        let delivery_id = match next_command(&commands) {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected SendTurn, got {other:?}"),
        };
        state.on_event(&id, AgentEvent::TurnAccepted { delivery_id }, cx);
        state.on_event(
            &id,
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            cx,
        );
        assert!(state.compact_context(&id, cx).is_err());
        assert!(
            state
                .change_goal(&id, SessionCommand::ClearGoal, cx)
                .is_err()
        );
        assert!(next_command(&commands).is_err());

        state.on_event(
            &id,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        state.compact_context(&id, cx).unwrap();
        assert!(matches!(
            next_command(&commands),
            Ok(SessionCommand::Compact)
        ));
        state
            .change_goal(&id, SessionCommand::ClearGoal, cx)
            .unwrap();
        assert!(matches!(
            next_command(&commands),
            Ok(SessionCommand::ClearGoal)
        ));
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

/// An `ActiveSession` wired to a fake live provider: commands land on the
/// returned receiver, nothing real is spawned.
fn fake_live_session(cwd: PathBuf) -> (ActiveSession, smol::channel::Receiver<SessionCommand>) {
    let (commands, receiver) = smol::channel::unbounded();
    let mut session = AppState::build_draft_session(
        "proj-t3".into(),
        cwd,
        ProviderKind::ClaudeCode,
        None,
        None,
        Vec::new(),
    );
    session.draft = false;
    session.runtime = Runtime::Live(commands);
    // What `ensure_started` records at launch — without these, `send_turn`
    // sees a live-config mismatch and restarts the provider instead of
    // dispatching.
    session.live_model = session.meta.model.clone();
    session.live_option_selections = session.meta.option_selections.clone();
    (session, receiver)
}

fn persisted_assistant_event(text: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: format!("item-{text}"),
        parent_item_id: None,
        content: ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        },
    })
}

#[test]
fn cold_select_installs_immediately_then_loads_persisted_timeline() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-cold-select-async-test");
    let store = (*test_store).clone();
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/cold"), None);
    store.upsert_meta(&meta).unwrap();
    store
        .append_event(
            &meta.id,
            1,
            &persisted_assistant_event("persisted cold output"),
        )
        .unwrap();
    let id = meta.id.clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        state.select_session(&id, cx);
        let active = state.selected_session().unwrap();
        assert_eq!(active.meta.id, id);
        assert!(active.timeline.entries.is_empty());
    });

    cx.run_until(|state| {
        state.selected_session().is_some_and(|session| {
            session
                .timeline
                .entries
                .iter()
                .any(|entry| entry.id == "item-persisted cold output")
        })
    });

    state.update(cx, |state, _| {
        assert!(state.selected_session().unwrap().timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "persisted cold output")
        ));
    });
}

#[test]
fn parked_readopt_refolds_events_appended_while_parked() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-parked-readopt-async-test");
    let store = (*test_store).clone();
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/parked"), None);
    store.upsert_meta(&meta).unwrap();
    store
        .append_event(&meta.id, 1, &persisted_assistant_event("before parking"))
        .unwrap();
    let id = meta.id.clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| state.select_session(&id, cx));
    cx.run_until(|state| {
        state.selected_session().is_some_and(|session| {
            session
                .timeline
                .entries
                .iter()
                .any(|entry| entry.id == "item-before parking")
        })
    });
    state.update(cx, |state, cx| {
        let active = state.selected_session_mut().unwrap();
        active.turn_in_flight = true;
        active.runtime = Runtime::Starting { generation: 1 };
        state.start_draft("other".into(), PathBuf::from("/tmp/other"), cx);
        state.record_event(&id, &persisted_assistant_event("while parked"), cx);
        state.select_session(&id, cx);
    });

    cx.run_until(|state| {
        state.selected_session().is_some_and(|session| {
            session
                .timeline
                .entries
                .iter()
                .any(|entry| entry.id == "item-while parked")
        })
    });

    state.update(cx, |state, _| {
        assert!(state.selected_session().unwrap().timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "while parked")
        ));
    });
}

#[test]
fn stale_timeline_completion_cannot_land_on_another_session() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-stale-timeline-load-test");
    let store = (*test_store).clone();
    let a = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/a"), None);
    let b = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/b"), None);
    store.upsert_meta(&a).unwrap();
    store.upsert_meta(&b).unwrap();
    store
        .append_event(&a.id, 1, &persisted_assistant_event("only session A"))
        .unwrap();
    store
        .append_event(&b.id, 1, &persisted_assistant_event("only session B"))
        .unwrap();
    let (id_a, id_b) = (a.id.clone(), b.id.clone());
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        state.select_session(&id_a, cx);
        state.select_session(&id_b, cx);
        assert_eq!(state.active_session_id(), Some(id_b.as_str()));
    });

    cx.run_until(|state| {
        state.selected_session().is_some_and(|session| {
            session
                .timeline
                .entries
                .iter()
                .any(|entry| entry.id == "item-only session B")
        })
    });

    state.update(cx, |state, _| {
        let active = state.selected_session().unwrap();
        assert_eq!(active.meta.id, id_b);
        assert!(active.timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "only session B")
        ));
        assert!(!active.timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "only session A")
        ));
    });
}

#[test]
fn timeline_load_keeps_records_appended_during_the_fold() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-timeline-watermark-test");
    let store = (*test_store).clone();
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/watermark"), None);
    store.upsert_meta(&meta).unwrap();
    store
        .append_event(&meta.id, 1, &persisted_assistant_event("before load"))
        .unwrap();
    let id = meta.id.clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        state.select_session(&id, cx);
        state.record_event(&id, &persisted_assistant_event("raced append"), cx);
    });

    cx.run_until(|state| {
        state.selected_session().is_some_and(|session| {
            session
                .timeline
                .entries
                .iter()
                .any(|entry| entry.id == "item-before load")
        })
    });

    state.update(cx, |state, _| {
        let timeline = &state.selected_session().unwrap().timeline;
        assert!(timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "before load")
        ));
        assert!(timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "raced append")
        ));
    });
}

/// After stopping one thread, the next thread's first accepted message must
/// remain visible and durable without inheriting the interrupted thread's error.
#[test]
fn stop_then_new_thread_keeps_the_first_message_visible() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-t3-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-t3-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    // Session A, live (fake provider: commands land on `commands_a`).
    let (session, commands_a) = fake_live_session(cwd.clone());
    let (commands_b, receiver_b) = smol::channel::unbounded();
    let mut id_b = String::new();

    state.update(cx, |state, cx| {
        // No real provider may spawn if a start slips through.
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .binary_path = Some("/nonexistent/tcode-test-claude".into());

        // Send → the provider command is queued, then the adapter's
        // acceptance commits the user bubble.
        state.install_selected(session);
        state.host.send_turn(state.selected.as_deref().unwrap_or_default(), "first message".into(), Vec::new(), cx);
        let id_a = state.selected_session().unwrap().meta.id.clone();
        let first_delivery = match commands_a.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected first SendTurn, got {other:?}"),
        };
        state.on_event(
            &id_a,
            AgentEvent::TurnAccepted {
                delivery_id: first_delivery,
            },
            cx,
        );
        assert!(state.selected_session().unwrap().timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "first message")
        ));

        state.on_event(
            &id_a,
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            cx,
        );

        // Stop. The provider reports an error and an interrupted turn while
        // preserving the complete multi-line error for later presentation.
        state.host.interrupt(state.selected.as_deref().unwrap_or_default(), cx).expect("interrupt delivered");
        assert!(matches!(
            commands_a.try_recv(),
            Ok(SessionCommand::Interrupt)
        ));
        state.on_event(
            &id_a,
            AgentEvent::Error {
                message: "Request was aborted\nwith a second line the toast never showed"
                    .into(),
                fatal: false,
            },
            cx,
        );
        state.on_event(
            &id_a,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Interrupted,
                usage: None,
            },
            cx,
        );

        // Immediately: new thread, send. The draft commits to a NEW session;
        // the message waits in the queue while the provider starts (still
        // visible in the queue strip — never dropped).
        state.start_draft("proj-t3".into(), cwd.clone(), cx);
        state.host.send_turn(state.selected.as_deref().unwrap_or_default(), "second message".into(), Vec::new(), cx);
        let active = state.selected_session().unwrap();
        id_b = active.meta.id.clone();
        assert_ne!(id_a, id_b);
        assert_eq!(active.queue.len(), 1);

        // Provider comes up (simulated — the queue flush on start).
        state.selected_session_mut().unwrap().runtime = Runtime::Live(commands_b);
        assert_eq!(state.host.dispatch_next_queued(state.selected.as_deref().unwrap_or_default(), cx), Ok(true));
        let second_delivery = match receiver_b.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected second SendTurn, got {other:?}"),
        };
        state.on_event(
            &id_b,
            AgentEvent::TurnAccepted {
                delivery_id: second_delivery,
            },
            cx,
        );

        // THE assertion: the new thread's first message is a visible user
        // entry in a rendered turn, and session A's error did not leak in.
        let active = state.selected_session().unwrap();
        let users: Vec<&str> = active
            .timeline
            .entries
            .iter()
            .filter_map(|e| match &e.content {
                EntryContent::Item(ItemContent::UserMessage { text, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec!["second message"]);
        let entry_turn = active.timeline.entries[0].turn;
        assert!(
            entry_turn < active.timeline.turns.len(),
            "user entry must belong to a rendered turn (turn {entry_turn} of {})",
            active.timeline.turns.len()
        );
        assert!(
            !active
                .timeline
                .entries
                .iter()
                .any(|e| matches!(e.content, EntryContent::Error { .. })),
            "session A's interrupt error leaked into the new thread"
        );
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        // And it is durable: a replay of the JSONL shows the same thing.
        let replayed = Timeline::fold_events(state.store.read_events(&id_b).unwrap());
        assert!(replayed.entries.iter().any(
            |e| matches!(&e.content, EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "second message")
        ));
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

/// A user message is activity: once the provider accepts it the thread's
/// updated time moves, which is what lifts it in the threads list.
#[test]
fn an_accepted_user_message_moves_the_thread_updated_time() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-message-activity-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-message-activity-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (mut session, commands) = fake_live_session(cwd.clone());
    session.meta.title = "Tidy the parser".into();
    session.meta.updated_at = 100;
    let id = session.meta.id.clone();

    state.update(cx, |state, cx| {
        state.store.upsert_meta(&session.meta).unwrap();
        state.sessions = state.store.load_index().unwrap().into();
        state.install_selected(session);
        state.send_turn(&id, "one more change".into(), Vec::new(), cx);
        let delivery_id = match commands.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected SendTurn, got {other:?}"),
        };
        assert_eq!(state.find_meta(&id).unwrap().updated_at, 100);
        state.on_event(&id, AgentEvent::TurnAccepted { delivery_id }, cx);
        assert!(state.find_meta(&id).unwrap().updated_at >= now_secs() - 60);
    });
    cx.run_until_parked();
    let persisted = test_store.load_index().unwrap();
    assert!(persisted[0].updated_at >= now_secs() - 60);

    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn submitted_queue_head_cannot_leak_delivery_after_turn_completion() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-submitted-drop-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-submitted-drop-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (session, commands) = fake_live_session(cwd.clone());
    let id = session.meta.id.clone();

    state.update(cx, |state, cx| {
        state.store.upsert_meta(&session.meta).unwrap();
        state.sessions = state.store.load_index().unwrap().into();
        state.install_selected(session);
        state.send_turn(&id, "finish this task".into(), Vec::new(), cx);
        let delivery_id = match commands.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected SendTurn, got {other:?}"),
        };

        // The submitted head remains in the visible queue strip until its
        // provider acknowledgement. Its ✕ must not invalidate correlation.
        state.drop_queued(&id, delivery_id, cx);
        state.on_event(&id, AgentEvent::TurnAccepted { delivery_id }, cx);
        state.on_event(
            &id,
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            cx,
        );
        state.on_event(
            &id,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(!state.turn_running_for(&id));

        state.start_draft("proj".into(), cwd.clone(), cx);
        state.select_session(&id, cx);
        assert!(!state.turn_running_for(&id));
        state.start_draft("proj".into(), cwd.clone(), cx);
        assert!(
            !state.turn_running_for(&id),
            "the completed delivery became Working again after reparking"
        );
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn event_stream_end_closes_only_its_own_live_provider_in_any_residency() {
    for parked in [false, true] {
        for (live, current) in [(true, true), (true, false), (false, false)] {
            let cx = &mut TestAppContext::default();
            let store = TestStore::new("tcode-stream-ownership");
            let state = cx.new_entity(TestClientState::new((*store).clone()));
            let (commands, _receiver) = smol::channel::unbounded();
            let (old_commands, _old_receiver) = smol::channel::unbounded();
            state.update(cx, |state, cx| {
                let mut session = live_session(ProviderKind::ClaudeCode, commands.clone());
                session.meta.id = "session".into();
                session.turn_in_flight = live;
                session.background_task_count = if live { 2 } else { 0 };
                if !live {
                    session.runtime = Runtime::Idle;
                }
                state.store.upsert_meta(&session.meta).unwrap();
                state.sessions.push(session.meta.clone());
                if parked {
                    state.residents.parked.insert("session".into(), session);
                } else {
                    state.install_selected(session);
                }
                state.on_event_stream_ended(
                    "session",
                    if current { &commands } else { &old_commands },
                    cx,
                );
                assert_eq!(
                    state.turn_running_for("session"),
                    live && !current,
                    "parked={parked}, live={live}, current={current}"
                );
                if let Some(session) = state.resident("session") {
                    assert_eq!(
                        matches!(session.runtime, Runtime::Live(_)),
                        live && !current
                    );
                } else {
                    assert!(parked && current);
                }
            });
            cx.run_until_parked();
            let closes = store
                .read_events("session")
                .unwrap()
                .iter()
                .filter(|stored| matches!(stored.event, AgentEvent::SessionClosed { .. }))
                .count();
            assert_eq!(
                closes,
                usize::from(live && current),
                "only the owning stream may persist a close"
            );
        }
    }
}

#[test]
fn turn_running_for_is_independent_of_active_or_parked_location() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-working-location-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let commands = smol::channel::unbounded().0;

    let mut idle = live_session(ProviderKind::ClaudeCode, commands.clone());
    idle.meta.id = "idle".into();

    let mut turn = live_session(ProviderKind::ClaudeCode, commands.clone());
    turn.meta.id = "turn".into();
    turn.turn_in_flight = true;

    let mut delivery = live_session(ProviderKind::ClaudeCode, commands.clone());
    delivery.meta.id = "delivery".into();
    delivery.delivery_in_flight = Some(7);
    delivery.background_task_count = 1;

    let mut queued = live_session(ProviderKind::ClaudeCode, commands.clone());
    queued.meta.id = "queued".into();
    queued.push_queued("waiting".into(), Vec::new());

    let mut background = live_session(ProviderKind::ClaudeCode, commands.clone());
    background.meta.id = "background".into();
    background.background_task_count = 1;

    let mut stale_timeline = live_session(ProviderKind::ClaudeCode, commands);
    stale_timeline.meta.id = "stale-timeline".into();
    stale_timeline.timeline.apply_at(
        None,
        &AgentEvent::TurnStarted {
            turn_id: "stale".into(),
        },
    );

    state.update(cx, |state, _| {
        for (label, session, expected) in [
            ("idle", idle, false),
            ("turn", turn, true),
            ("delivery", delivery, true),
            ("queued", queued, true),
            ("background", background, true),
            ("stale timeline", stale_timeline, false),
        ] {
            let id = session.meta.id.clone();
            let expected_background = matches!(label, "delivery" | "background");
            state.sessions.push(session.meta.clone());
            state.install_selected(session);
            let index = state.index_snapshot();
            assert_eq!(
                index.summary.activity[&id].background.tasks > 0,
                expected_background,
                "{label}"
            );
            let active_answer = state.turn_running_for(&id);
            // Background work keeps the provider resident and shows as such;
            // Working describes this thread's own turn or queued human input.
            let expected_working = expected && label != "background";
            assert_eq!(
                state.session_status_snapshot(&id).unwrap().activity.working,
                expected_working,
                "{label}"
            );

            let parked = state.take_selected().unwrap();
            state.residents.parked.insert(id.clone(), parked);
            let parked_answer = state.turn_running_for(&id);
            assert_eq!(
                state.session_status_snapshot(&id).unwrap().activity.working,
                expected_working,
                "{label}"
            );

            assert_eq!(
                active_answer, parked_answer,
                "{label} changed answer when moved between active and parked"
            );
            assert_eq!(active_answer, expected, "{label} work predicate");
            state.residents.parked.remove(&id);
        }
    });
}

/// A running grandchild keeps every thread above it running an agent, not
/// only its own lead, though none of them has work of its own.
#[test]
fn a_running_grandchild_keeps_every_ancestor_running_an_agent() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-waiting-child-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let commands = smol::channel::unbounded().0;

    let mut parent = live_session(ProviderKind::ClaudeCode, commands.clone());
    parent.meta.id = "parent".into();
    let mut child = live_session(ProviderKind::ClaudeCode, commands.clone());
    child.meta.id = "child".into();
    child.meta.parent_session_id = Some("parent".into());
    let mut grandchild = live_session(ProviderKind::ClaudeCode, commands);
    grandchild.meta.id = "grandchild".into();
    grandchild.meta.parent_session_id = Some("child".into());
    grandchild.turn_in_flight = true;

    state.update(cx, |state, _| {
        for session in [&parent, &child, &grandchild] {
            state.sessions.push(session.meta.clone());
        }
        state.install_selected(parent);
        state.residents.parked.insert("child".into(), child);
        state
            .residents
            .parked
            .insert("grandchild".into(), grandchild);

        let activity = state.index_snapshot().summary.activity;
        assert!(activity["parent"].background.agents == 1 && !activity["parent"].working);
        assert!(activity["child"].background.agents == 1 && !activity["child"].working);
        assert!(activity["grandchild"].background.is_idle() && activity["grandchild"].working);
    });
}

/// Switching to another thread must not kill a session whose turn is still
/// running. The session parks in the background — process and queue alive,
/// events still recorded, sidebar still "Working" — and selecting it again
/// re-adopts it with the streamed-while-parked content visible.
#[test]
fn switching_threads_parks_a_working_session_instead_of_killing_it() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-park-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-park-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    // A live session (fake provider: commands land on `commands_a`).
    let (session, commands_a) = fake_live_session(cwd.clone());
    let id_a = session.meta.id.clone();

    state.update(cx, |state, cx| {
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .binary_path = Some("/nonexistent/tcode-test-claude".into());

        // A live session with a running turn (the overnight workflow).
        state.store.upsert_meta(&session.meta).unwrap();
        state.sessions = state.store.load_index().unwrap().into();
        state.install_selected(session);
        state.send_turn(&id_a, "run the long migration".into(), Vec::new(), cx);
        state.send_turn(&id_a, "queued follow-up".into(), Vec::new(), cx);
        let first_delivery = match next_command(&commands_a) {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected migration SendTurn, got {other:?}"),
        };
        state.on_event(
            &id_a,
            AgentEvent::TurnAccepted {
                delivery_id: first_delivery,
            },
            cx,
        );
        state.on_event(
            &id_a,
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            cx,
        );

        // Glance at another thread: the session must survive, not die.
        state.start_draft("proj-t3".into(), cwd.clone(), cx);
        assert!(
            next_command(&commands_a).is_err(),
            "switching threads must not send Shutdown to a working session"
        );
        assert!(
            state.turn_running_for(&id_a),
            "a parked working session keeps its sidebar Working status"
        );

        // The parked session keeps streaming; its events keep landing in
        // the JSONL even though another thread is on screen.
        state.on_event(
            &id_a,
            AgentEvent::ItemCompleted(ThreadItem {
                id: "bg-1".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "Migration step 1 done.".into(),
                    summarized: false,
                },
            }),
            cx,
        );

        // Its turn completes in the background → the queued follow-up goes
        // out as the next turn, on the same process.
        state.on_event(
            &id_a,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        let follow_up_delivery = match next_command(&commands_a) {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected follow-up SendTurn, got {other:?}"),
        };
        state.on_event(
            &id_a,
            AgentEvent::TurnAccepted {
                delivery_id: follow_up_delivery,
            },
            cx,
        );
        assert!(state.turn_running_for(&id_a));

        // Coming back re-adopts the live session: everything that happened
        // while parked is in the timeline, and the turn is still running.
        state.select_session(&id_a, cx);
    });

    cx.run_until_parked();

    state.update(cx, |state, cx| {
        let active = state.selected_session().unwrap();
        assert_eq!(active.meta.id, id_a);
        assert!(matches!(active.runtime, Runtime::Live(_)));
        assert!(active.turn_in_flight);
        assert!(active.timeline.entries.iter().any(|e| matches!(
            &e.content,
            EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "Migration step 1 done."
        )));
        assert!(active.timeline.entries.iter().any(|e| matches!(
            &e.content,
            EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "queued follow-up"
        )));

        state.on_event(
            &id_a,
            AgentEvent::TurnCompleted {
                turn_id: "turn-2".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(
            !state.turn_running_for(&id_a),
            "a completed active session must not remain Working"
        );
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

/// A parked session that runs out of work becomes an idle resident instead
/// of immediately rebuilding its provider on the next selection.
#[test]
fn drained_parked_session_stays_resident() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-parkend-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-parkend-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    // A live session (fake provider: commands land on `commands`).
    let (session, commands) = fake_live_session(cwd.clone());
    let id = session.meta.id.clone();

    state.update(cx, |state, cx| {
        state.store.upsert_meta(&session.meta).unwrap();
        state.sessions = state.store.load_index().unwrap().into();
        state.install_selected(session);
        state.send_turn(&id, "one last thing".into(), Vec::new(), cx);
        let delivery_id = match next_command(&commands) {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected final SendTurn, got {other:?}"),
        };
        state.on_event(&id, AgentEvent::TurnAccepted { delivery_id }, cx);

        state.start_draft("proj".into(), cwd.clone(), cx);
        assert!(state.turn_running_for(&id));

        // The parked turn finishes with an empty queue → resident grace.
        state.on_event(
            &id,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(next_command(&commands).is_err(), "drain sent Shutdown");
        assert!(state.residents.parked.contains_key(&id));
        assert!(state.residents.parked[&id].idle_since.is_some());
        assert!(!state.turn_running_for(&id));
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

/// A failed provider start must not destroy what the user typed: the queued
/// message stays in the queue (visible in the strip, flushed by the next
/// successful start) instead of being cleared.
#[test]
fn failed_provider_start_keeps_the_queued_message() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-t3f-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-t3f-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        // A binary that cannot exist → start_session fails fast.
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .binary_path = Some("/nonexistent/tcode-test-claude".into());
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .display_name = Some("tryanks".into());
        state.start_draft("proj-fail".into(), cwd.clone(), cx);
        state.host.send_turn(
            state.selected.as_deref().unwrap_or_default(),
            "do not lose me".into(),
            Vec::new(),
            cx,
        );
        assert_eq!(state.selected_session().unwrap().queue.len(), 1);
    });

    cx.run_until(|state| {
        state
            .selected_session()
            .is_some_and(|session| matches!(session.runtime, Runtime::Idle))
    });

    state.update(cx, |state, _| {
        let active = state.selected_session().unwrap();
        assert!(
            matches!(active.runtime, Runtime::Idle),
            "failed start must return to Idle"
        );
        assert_eq!(
            active.queue.first().map(|m| m.text.as_str()),
            Some("do not lose me"),
            "the user's text must survive a failed provider start"
        );
        // The failure itself is on the record.
        assert!(
            active
                .timeline
                .entries
                .iter()
                .any(|e| matches!(e.content, EntryContent::ProviderStartError { .. })),
            "the start failure must be recorded in the timeline"
        );
    });
    assert!(
        cx.drain_outgoing().iter().any(|message| matches!(
            message,
            HostMessage::Event(EventEnvelope {
                event: ServerEvent::Runtime(RuntimeEvent::Error(RuntimeError::ProviderStart {
                    harness,
                    ..
                })),
                ..
            }) if harness == "Claude Code (tryanks)"
        )),
        "the failure names the Harness and its account"
    );

    let _ = std::fs::remove_dir_all(&cwd);
}

fn recv_dispatch_reply<T>(cx: &mut TestAppContext, rx: &smol::channel::Receiver<T>) -> T {
    let mut reply = None;
    cx.run_until(|_| {
        reply = rx.try_recv().ok();
        reply.is_some()
    });
    reply.expect("dispatch reply observed above")
}

/// A repository at `root` on `main` with one commit of `tracked.txt`.
fn init_git_repo(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    for args in [
        &["init", "-b", "main"][..],
        &["config", "commit.gpgsign", "false"],
        &["config", "core.hooksPath", ".no-hooks"],
        &["config", "core.autocrlf", "false"],
        &["config", "user.name", "tcode"],
        &["config", "user.email", "tcode@localhost"],
    ] {
        run_git(root, args).unwrap();
    }
    std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
    run_git(root, &["add", "tracked.txt"]).unwrap();
    run_git(root, &["commit", "-m", "initial"]).unwrap();
}

/// A draft in worktree mode starts its worktree from the base picked in the composer, not
/// from what the project root has checked out.
#[test]
fn a_worktree_draft_starts_from_the_picked_base() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-worktree-picked-base");
    let repo = test_store.root().join("repo");
    init_git_repo(&repo);
    run_git(&repo, &["switch", "-c", "feature"]).unwrap();
    std::fs::write(repo.join("feature.txt"), "feature\n").unwrap();
    run_git(&repo, &["add", "feature.txt"]).unwrap();
    run_git(&repo, &["commit", "-m", "feature"]).unwrap();
    run_git(&repo, &["switch", "main"]).unwrap();
    let project = Project::from_root(repo.clone());
    test_store.upsert_project(&project).unwrap();
    let (launcher, launches) = recording_launcher();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher_for_test(launcher);
        state
    });

    let Ok(CommandResponse::SessionId(Some(session_id))) = acked(
        &state,
        cx,
        1,
        Command::StartDraft {
            project_id: project.id.clone(),
            cwd: repo.clone(),
        },
    ) else {
        panic!("draft not started");
    };
    for (id, command) in [
        Command::SetDraftWorkspace {
            session_id: session_id.clone(),
            mode: WorkspaceMode::NewWorktree {
                base: "feature".into(),
            },
        },
        Command::SendTurn {
            session_id: session_id.clone(),
            text: "build on the feature".into(),
            attachment_paths: Vec::new(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        acked(&state, cx, id as u64 + 2, command).unwrap();
    }
    next_turn(cx, &launches, 0);

    let cwd = launches.lock().unwrap()[0].0.cwd.clone();
    assert_ne!(cwd, repo);
    assert_eq!(
        std::fs::read_to_string(cwd.join("feature.txt")).unwrap(),
        "feature\n"
    );
    state.read(|state| {
        let meta = state.find_meta(&session_id).unwrap();
        assert_eq!(
            meta.worktree.map(|worktree| worktree.base).as_deref(),
            Some("feature")
        );
    });
    remove_git_worktree(&repo, &cwd).unwrap();
}

/// A thread may move into a worktree of its project's repository that lives outside the
/// project's directory; a directory of no checkout of that repository is still refused. A
/// project rooted in a subdirectory of its repository reaches only that subdirectory of
/// each checkout.
#[test]
fn a_thread_moves_into_a_worktree_of_its_projects_repository() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-move-into-worktree");
    let repo = test_store.root().join("repo");
    init_git_repo(&repo);
    std::fs::create_dir(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/tracked.txt"), "sub\n").unwrap();
    run_git(&repo, &["add", "sub"]).unwrap();
    run_git(&repo, &["commit", "-m", "sub"]).unwrap();
    let outside = test_store.root().join("outside-worktree");
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "elsewhere",
            outside.to_str().unwrap(),
        ],
    )
    .unwrap();
    std::fs::create_dir(outside.join("app")).unwrap();
    std::fs::create_dir(outside.join("other")).unwrap();
    let unrelated = test_store.root().join("unrelated");
    std::fs::create_dir_all(&unrelated).unwrap();
    let project = Project::from_root(repo.clone());
    test_store.upsert_project(&project).unwrap();
    let sub_project = Project::from_root(repo.join("sub"));
    test_store.upsert_project(&sub_project).unwrap();
    for (id, project, cwd) in [
        ("thread", &project, &repo),
        ("sub-thread", &sub_project, &repo.join("sub")),
    ] {
        let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, cwd.clone(), None);
        meta.id = id.into();
        meta.project_id = Some(project.id.clone());
        test_store.upsert_meta(&meta).unwrap();
    }
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let move_to = |thread: &str, project: &Project, cwd: &Path| Command::MoveSession {
        session_id: thread.into(),
        project_id: project.id.clone(),
        cwd: Some(cwd.to_path_buf()),
    };

    let refusal = acked(&state, cx, 1, move_to("thread", &project, &unrelated)).unwrap_err();
    assert_eq!(refusal.code, "cwd_outside_project");
    acked(
        &state,
        cx,
        2,
        move_to("thread", &project, &outside.join("app")),
    )
    .unwrap();
    state.read(|state| {
        assert_eq!(state.find_meta("thread").unwrap().cwd, outside.join("app"));
    });

    for (id, cwd) in [(3, outside.join("other")), (4, repo.clone())] {
        let refusal = acked(&state, cx, id, move_to("sub-thread", &sub_project, &cwd)).unwrap_err();
        assert_eq!(refusal.code, "cwd_outside_project", "{}", cwd.display());
    }
    acked(
        &state,
        cx,
        5,
        move_to("sub-thread", &sub_project, &outside.join("sub")),
    )
    .unwrap();
    state.read(|state| {
        assert_eq!(
            state.find_meta("sub-thread").unwrap().cwd,
            outside.join("sub")
        );
    });
}

#[test]
fn a_subagent_started_in_a_worktree_works_in_it() {
    let cx = &mut TestAppContext::default();
    let root =
        std::env::temp_dir().join(format!("tcode-dispatch-worktree-{}", uuid::Uuid::new_v4()));
    init_git_repo(&root);

    let test_store = TestStore::new("tcode-dispatch-worktree-data");
    let data_worktrees = test_store.root().join("worktrees");
    let scripted = scripted_provider(ProviderKind::Codex);
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher_for_test(scripted.launcher);
        state
    });
    let (reply, response) = smol::channel::bounded(1);

    state.update(cx, |state, cx| {
        let project_id = state.create_project(root.clone(), cx).unwrap();
        let mut parent = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        parent.id = "parent".into();
        parent.project_id = Some(project_id);
        state.sessions.push(parent);
        state.handle_control_op(
            "parent".into(),
            control_mcp::ControlOp::StartThread(control_mcp::StartThreadParams {
                project_id: None,
                cwd: None,
                workspace: Some(control_mcp::Workspace::Worktree),
                subagent: Some(true),
                provider: "codex".into(),
                model: Some("gpt-6.1-sol".into()),
                profile: None,
                options: None,
                title: Some("Isolated child".into()),
                message: "Inspect the workspace".into(),
                attachments: None,
                request_id: "isolated".into(),
            }),
            reply,
            cx,
        );
    });
    let response = recv_dispatch_reply(cx, &response).unwrap();
    let child_id = response["thread_id"].as_str().unwrap().to_string();
    let expected_branch = format!("tcode/{child_id}");
    let expected_path = PathBuf::from(response["cwd"].as_str().unwrap());
    assert!(
        expected_path.starts_with(&data_worktrees) && expected_path.ends_with(&child_id),
        "start_thread response: {response}"
    );
    assert_eq!(response["worktree_branch"], expected_branch);
    state.update(cx, |state, _| {
        let child = state.find_meta(&child_id).unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(child.cwd, expected_path);
        assert_eq!(
            child
                .worktree
                .as_ref()
                .map(|worktree| worktree.branch.as_str()),
            Some(expected_branch.as_str())
        );
    });
    remove_git_worktree(&root, &expected_path).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn orphan_sweep_leaves_worktrees_a_thread_works_in_or_the_user_kept() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orphan-sweep");
    let store = (*test_store).clone();
    let repo = test_store.root().join("repo");
    init_git_repo(&repo);
    let owner = |id: &str| {
        let created = provision(&repo, id, None, store.root()).unwrap();
        let mut meta = SessionMeta::new(ProviderKind::Codex, created.path, None);
        meta.id = id.into();
        meta.worktree = Some(WorktreeInfo {
            root_project_path: repo.clone(),
            base: created.base,
            branch: created.branch,
        });
        meta
    };
    let source = owner("source");
    let kept = owner("kept");
    let orphan = owner("orphan").cwd;
    let mut fork = SessionMeta::new(ProviderKind::Codex, source.cwd.clone(), None);
    fork.id = "fork".into();
    // A thread that owns a worktree it does not work in now.
    let away = owner("away");
    let mut elsewhere = SessionMeta::new(ProviderKind::ClaudeCode, repo.clone(), None);
    elsewhere.id = "elsewhere".into();
    elsewhere
        .other_worktrees
        .push(tcode_core::project::OwnedWorktree {
            path: away.cwd.clone(),
            info: away.worktree.clone().unwrap(),
        });
    for meta in [&source, &kept, &fork, &elsewhere] {
        store.upsert_meta(meta).unwrap();
    }
    let state = cx.new_entity(TestClientState::new(store.clone()));

    state.dispatch_command(
        cx,
        1,
        Command::DeleteSession {
            session_id: "source".into(),
            remove_worktree: true,
        },
    );
    state.dispatch_command(
        cx,
        2,
        Command::DeleteSession {
            session_id: "kept".into(),
            remove_worktree: false,
        },
    );
    cx.run_until_parked();
    assert!(source.cwd.exists(), "the fork still works in it");
    // The next start is past the age that marks a worktree as abandoned.
    let abandoned = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - Duration::from_secs(2 * 60 * 60),
    );
    for path in [&source.cwd, &kept.cwd, &orphan, &away.cwd] {
        filetime::set_file_mtime(path, abandoned).unwrap();
    }

    let summary = cleanup_orphans(&store);

    assert_eq!(summary.removed, std::slice::from_ref(&orphan));
    assert!(!orphan.exists());
    assert!(source.cwd.join("tracked.txt").exists());
    assert!(kept.cwd.join("tracked.txt").exists());
    assert!(away.cwd.join("tracked.txt").exists());
}

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
    let launcher = ProviderLauncher(Arc::new(move |kind, options| {
        let launcher = if options.cwd.ends_with("one") {
            first.launcher.clone()
        } else {
            second.launcher.clone()
        };
        Box::pin(async move { launcher.launch(kind, options).await })
    }));
    let host = spawn_host((*root).clone(), HostServices::default()).unwrap();
    smol::block_on(
        host.update_state_for_test(move |state, _| state.set_provider_launcher_for_test(launcher)),
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
        let window = smol::block_on(smol::future::race(
            async { link.events().recv().await.unwrap() },
            async {
                smol::Timer::after(Duration::from_secs(5)).await;
                panic!("the subscription was not answered")
            },
        ));
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
        let command = smol::block_on(smol::future::race(
            async { commands.recv().await.unwrap() },
            async {
                smol::Timer::after(Duration::from_secs(5)).await;
                panic!("provider did not receive turn")
            },
        ));
        let SessionCommand::SendTurn { text, .. } = command else {
            panic!("expected send")
        };
        assert_eq!(text, expected);
    };
    receive_turn(&first_commands, "from desktop");
    receive_turn(&second_commands, "from phone");
    let ids = (id_one.clone(), id_two.clone());
    smol::block_on(host.update_state_for_test(move |state, cx| {
        for id in [&ids.0, &ids.1] {
            for text in ["first", "second", "third"] {
                state.record_event(
                    id,
                    &AgentEvent::Warning {
                        message: text.into(),
                    },
                    cx,
                );
            }
        }
    }))
    .unwrap();
    for (link, id) in [(&one, &id_one), (&two, &id_two)] {
        let events = link.events();
        let mut received = Vec::new();
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
        state.delete_session("fork", false, cx);
        let after = state.archived_sessions();
        assert!(after.revision > before.revision);
        assert!(!after.worktree_shared.contains("owner"));
        let draft = state
            .host
            .start_draft(project.id.clone(), owner.cwd.clone(), cx);
        let shared = state.archived_sessions();
        assert!(shared.revision > after.revision);
        assert!(shared.worktree_shared.contains("owner"));
        state.shutdown_active(&draft, cx);
        let unshared = state.archived_sessions();
        assert!(unshared.revision > shared.revision);
        assert!(!unshared.worktree_shared.contains("owner"));
    });
}

#[test]
fn usage_projection_survives_a_partial_history_window() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("session-plan-usage-window");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let id = "plan-usage";
    state.update(cx, |state, cx| {
        let mut meta = SessionMeta::new(
            ProviderKind::ClaudeCode,
            store.root().clone(),
            Some("claude-sonnet-4-6".into()),
        );
        meta.id = id.into();
        meta.option_selections = vec![OptionSelection {
            id: "contextWindow".into(),
            value: serde_json::json!(500_000),
        }];
        state.install_selected(ActiveSession::new(meta, false, Vec::new()));
        state.on_event(
            id,
            AgentEvent::TurnStarted {
                turn_id: "first".into(),
            },
            cx,
        );
        state.on_event(
            id,
            AgentEvent::TokenUsage(agent::TokenUsage {
                context_window: Some(1_000_000),
                used_tokens: Some(1234),
                total_processed_tokens: Some(9876),
                freshness: agent::ContextFreshness::Current,
                ..Default::default()
            }),
            cx,
        );
        state.on_event(
            id,
            AgentEvent::TurnCompleted {
                turn_id: "first".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        state.on_event(
            id,
            AgentEvent::TurnStarted {
                turn_id: "second".into(),
            },
            cx,
        );
        for index in 0..401 {
            state.on_event(
                id,
                AgentEvent::Warning {
                    message: format!("later record {index}"),
                },
                cx,
            );
        }
    });
    cx.run_until(|state| state.event_records.contains_key(id));
    state.update(cx, |state, _| {
        let snapshot = events_reply(
            state,
            &Subscription {
                known_index: None,
                topic: Topic::SessionEvents {
                    session_id: id.into(),
                },
                after: None,
            },
        );
        let ServerEvent::SessionSnapshot { from, records, .. } = snapshot.event else {
            panic!("history snapshot")
        };
        assert!(from > 0);
        let partial = Timeline::fold_events(records);
        assert!(partial.usage.is_none());
        let status = state.session_status_snapshot(id).unwrap();
        let usage = status.usage.unwrap();
        assert_eq!(usage.used_tokens, Some(1234));
        assert_eq!(usage.total_processed_tokens, Some(9876));
        assert_eq!(usage.context_window, Some(1_000_000));
        assert_eq!(status.context_window, Some(500_000));
    });
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
    // Each of the three long warnings takes most of half a reply.
    let long = |index: usize| format!("{index}{}", "x".repeat(200 * 1024));
    let messages: Vec<String> = ["first".to_owned()]
        .into_iter()
        .chain((0..3).map(long))
        .chain(["last".to_owned()])
        .collect();
    let records = messages
        .iter()
        .enumerate()
        .map(|(ts, message)| SessionEventRecord {
            origin: None,
            author: None,
            ts: Some(ts as u64),
            event: AgentEvent::Warning {
                message: message.clone(),
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
    let warning = |ts: usize| serde_json::json!({"ts": ts, "event": {"type": "warning", "message": messages[ts]}});

    // The client held the first row; the three long warnings and the last
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
                "records": [warning(2), warning(3), warning(4)],
                "total": 5, "total_turns": 0, "truncated": true,
            }},
        }}),
    );

    // Held through the second long warning, the rest fits.
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
                "records": [warning(3), warning(4)],
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
        let records: Vec<SessionEventRecord> = (0..2000)
            .map(|index| SessionEventRecord {
                origin: None,
                author: None,
                ts: Some(index),
                event: AgentEvent::Warning {
                    message: format!("event {index}"),
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
        assert!(
            tcode_protocol::encode_line(&HostMessage::Event(snapshot.clone()))
                .unwrap()
                .len()
                <= tcode_protocol::MAX_SESSION_HISTORY_BYTES
        );
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
            } = state.event_records["large"]
                .history_page(before, 200)
                .unwrap()
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
            records.push(SessionEventRecord {
                origin: None,
                author: None,
                ts: Some(turn * 1000),
                event: AgentEvent::ItemCompleted(ThreadItem {
                    id: format!("user-{turn}"),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: format!("question {turn}"),
                        context_len: None,
                        attachments: Vec::new(),
                    },
                }),
                elided: None,
            });
            records.push(SessionEventRecord {
                origin: None,
                author: None,
                ts: Some(turn * 1000 + 1),
                event: AgentEvent::TurnStarted {
                    turn_id: format!("turn-{turn}"),
                },
                elided: None,
            });
            for delta in 0..300u64 {
                records.push(SessionEventRecord {
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
            records.push(SessionEventRecord {
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
        } = state.event_records["streamed"]
            .history_page(from, 200)
            .unwrap()
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

/// Persist `turns` streamed turns for `id` and return the records as the
/// store replays them.
fn persist_streamed_turns(store: &SessionStore, id: &str, turns: u64) -> Vec<SessionEventRecord> {
    for turn in 0..turns {
        let mut events = vec![
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("user-{turn}"),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: format!("question {turn}"),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            AgentEvent::TurnStarted {
                turn_id: format!("turn-{turn}"),
            },
        ];
        events.extend((0..300).map(|_| AgentEvent::Delta {
            item_id: "pi-assistant-0:0".into(),
            kind: agent::DeltaKind::AssistantText,
            text: "word ".into(),
        }));
        events.push(AgentEvent::TurnCompleted {
            turn_id: format!("turn-{turn}"),
            status: TurnStatus::Completed,
            usage: None,
        });
        let appends: Vec<_> = events
            .iter()
            .enumerate()
            .map(|(offset, event)| {
                tcode_services::store::Mutation::append_event(
                    id,
                    turn * 1000 + offset as u64,
                    event,
                )
                .unwrap()
            })
            .collect();
        store.apply(&appends).unwrap();
    }
    store.read_events(id).unwrap()
}

/// A client scrolling to the top of a long thread fetches one page per turn;
/// every page must come from the log parsed when the thread was opened.
#[test]
fn history_pages_of_an_opened_session_parse_the_log_once() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-single-parse");
    open_stored_session(&store, "paged");
    let records = persist_streamed_turns(&store, "paged", 5);
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let reads_before_open = store.event_reads();

    let window = window_reply(
        &through_pipe(&state, cx, 1, subscribe_events("paged", None)),
        1,
    );
    assert_eq!(
        window.total_turns, 5,
        "the absolute turn count comes from the log"
    );
    cx.run_until(|state| {
        state
            .resident("paged")
            .is_some_and(|session| session.timeline.turns.len() == 5)
    });
    let mut loaded = window.records;
    let mut before = window.from;
    let mut id = 2;
    while before > 0 {
        let QueryResponse::SessionHistoryPage {
            records: page,
            from,
            end,
            ..
        } = query_reply(
            &through_pipe(&state, cx, id, history_page("paged", before)),
            id,
        )
        else {
            panic!("page")
        };
        assert_eq!(end, before);
        loaded.splice(0..0, page);
        before = from;
        id += 1;
    }
    assert_eq!(
        Timeline::fold_events(loaded),
        Timeline::fold_events(records),
        "merged deltas fold as the log does"
    );
    assert_eq!(
        store.event_reads() - reads_before_open,
        1,
        "opening, loading the timeline and paging parsed the log once"
    );
}

/// The log of an opened session is read once, extended by its appends, and
/// kept while the session is resident, parked included; once it leaves
/// residency it is dropped only after the store writer flushed its appends,
/// so the next read finds every one of them.
#[test]
fn session_log_follows_residency_and_flushes_before_release() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-log-residency");
    open_stored_session(&store, "resident");
    let persisted = persist_streamed_turns(&store, "resident", 2).len() as u64;
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let reads_before_open = store.event_reads();
    let (commands, _actor) = smol::channel::unbounded();

    window_reply(
        &through_pipe(&state, cx, 1, subscribe_events("resident", None)),
        1,
    );
    state.update(cx, |state, cx| {
        state.record_event(
            "resident",
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "user-late".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "one more question".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            cx,
        )
    });
    let window = window_reply(
        &through_pipe(&state, cx, 2, subscribe_events("resident", Some(persisted))),
        2,
    );
    assert_eq!((window.total, window.total_turns), (persisted + 1, 3));
    assert!(
        matches!(&window.records[..], [record] if matches!(&record.event, AgentEvent::ItemCompleted(item) if item.id == "user-late"))
    );
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
    assert_eq!(
        store.event_reads() - reads_before_open,
        1,
        "the appended record is served from the log, not a re-read"
    );

    // Parked with a live provider, the session stays resident and cached.
    state.update(cx, |state, _| {
        state.residents.live.get_mut("resident").unwrap().runtime = Runtime::Live(commands);
    });
    through_pipe(&state, cx, 4, unsubscribe_events("resident"));
    state.update(cx, |state, cx| {
        assert!(state.residents.parked.contains_key("resident"));
        assert!(state.event_records.contains_key("resident"));

        state.drop_background("resident", cx);
        assert!(state.resident("resident").is_none());
        assert!(
            state.event_records.contains_key("resident"),
            "the log outlives residency until the store writer flushed its appends"
        );
    });
    cx.run_until(|state| !state.event_records.contains_key("resident"));
    assert_eq!(
        store.read_events("resident").unwrap().len() as u64,
        persisted + 1
    );
    assert_eq!(store.event_reads() - reads_before_open, 2);
}

#[test]
fn history_byte_budget_preserves_contiguous_records_and_reports_shrinking() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-byte-budget");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        let records: Vec<SessionEventRecord> = (0..10)
            .map(|index| SessionEventRecord {
                origin: None,
                author: None,
                ts: Some(index),
                event: AgentEvent::Warning {
                    message: "x".repeat(1024 * 1024),
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
        let mut snapshot = events_reply(state, &subscription);
        snapshot.request_id = Some(u64::MAX);
        assert!(
            tcode_protocol::encode_line(&HostMessage::Event(snapshot.clone()))
                .unwrap()
                .len()
                <= tcode_protocol::MAX_SESSION_HISTORY_BYTES
        );
        let ServerEvent::SessionSnapshot {
            from,
            records: tail,
            truncated,
            ..
        } = snapshot.event
        else {
            panic!("snapshot")
        };
        assert!(truncated);
        assert_eq!(tail, records[from as usize..]);
        let response = state.event_records["large"]
            .history_page(10, u32::MAX)
            .unwrap();
        let line = tcode_protocol::encode_line(&HostMessage::QueryResult {
            id: u64::MAX,
            result: Ok(response.clone()),
        })
        .unwrap();
        assert!(line.len() <= tcode_protocol::MAX_SESSION_HISTORY_BYTES);
        let QueryResponse::SessionHistoryPage {
            from,
            records: page,
            truncated,
            ..
        } = response
        else {
            panic!("page")
        };
        assert!(truncated);
        assert_eq!(page, records[from as usize..]);
        // A cursor whose rest overflows the budget gets that same tail.
        assert_eq!(
            events_reply(
                state,
                &tcode_protocol::Subscription {
                    after: Some(0),
                    ..subscription.clone()
                },
            )
            .event,
            events_reply(state, &subscription).event
        );
        let mut records = records;
        records[9].event = AgentEvent::Warning {
            message: "x".repeat(tcode_protocol::MAX_SESSION_HISTORY_BYTES),
        };
        state
            .event_records
            .insert("large".into(), session_log(records));
        assert_eq!(
            state.event_records["large"]
                .history_page(10, 1)
                .unwrap_err()
                .code,
            "history_record_too_large"
        );
    });
}

#[test]
fn computer_use_registrations_survive_stop_but_are_replaced_after_provider_shutdown() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-cu-registration-lifecycle");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let mut host = mcp_host::Host::bind().unwrap();
    let server = computer_use_mcp::start(&mut host);
    let (mut session, commands) = fake_live_session(std::env::temp_dir());
    // Stop is only offered while a turn runs.
    session.turn_in_flight = true;
    state.update(cx, |state, cx| {
        state
            .host
            .attach_computer_use_mcp(server.url, server.tokens);
        let meta = session.meta.clone();
        let other = SessionMeta::new(ProviderKind::Codex, std::env::temp_dir(), None);
        state.install_selected(session);
        let first = state.host.computer_use_registration_for(&meta).unwrap();
        let second = state.host.computer_use_registration_for(&other).unwrap();
        assert!(
            first.bearer_token != second.bearer_token,
            "provider sessions must not share one cancellation scope"
        );
        state.host.interrupt(&meta.id, cx).unwrap();
        assert!(matches!(commands.try_recv(), Ok(SessionCommand::Interrupt)));
        assert!(
            state
                .host
                .computer_use_registration_for(&meta)
                .unwrap()
                .bearer_token
                == first.bearer_token,
            "Stop permits the same provider to start its next turn"
        );
        state.host.shutdown_active(&meta.id, cx);
        assert!(matches!(commands.try_recv(), Ok(SessionCommand::Shutdown)));
        assert!(
            state
                .host
                .computer_use_registration_for(&other)
                .unwrap()
                .bearer_token
                == second.bearer_token,
            "another provider retains its registration"
        );
        assert!(
            state
                .host
                .computer_use_registration_for(&meta)
                .unwrap()
                .bearer_token
                != first.bearer_token,
            "a restarted provider must not inherit revoked credentials"
        );
        state.host.shutdown_all(cx);
        assert!(state.host.mcp.computer_use_registrations.is_empty());
    });
}

#[test]
fn interrupt_reports_stopping_until_the_turn_completes() {
    let store = TestStore::new("interrupt-stopping");
    let mut state = AppState::new((*store).clone()).unwrap();
    let context = TestAppContext::default();
    let mut cx = context.host_cx();
    let id = state.start_draft("fixture".into(), std::env::temp_dir(), &mut cx);
    let (commands, _receiver) = smol::channel::unbounded();
    let active = state.resident_mut(&id).unwrap();
    active.runtime = Runtime::Live(commands);
    active.turn_in_flight = true;
    assert!(!state.session_status_snapshot(&id).unwrap().stopping);

    state.interrupt(&id, &mut cx).unwrap();
    assert!(state.session_status_snapshot(&id).unwrap().stopping);

    state.on_event(
        &id,
        AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: TurnStatus::Interrupted,
            usage: None,
        },
        &mut cx,
    );
    let status = state.session_status_snapshot(&id).unwrap();
    assert!(!status.stopping);
    assert!(!status.activity.turn_running);
}

#[test]
fn status_carries_the_running_turn_while_in_flight_and_open_requests_while_live() {
    let store = TestStore::new("status-running-turn");
    let mut state = AppState::new((*store).clone()).unwrap();
    let context = TestAppContext::default();
    let mut cx = context.host_cx();
    let id = state.start_draft("fixture".into(), std::env::temp_dir(), &mut cx);
    let (commands, _receiver) = smol::channel::unbounded();
    state.resident_mut(&id).unwrap().runtime = Runtime::Live(commands);
    for turn in ["turn-1", "turn-2"] {
        state.on_event(
            &id,
            AgentEvent::TurnStarted {
                turn_id: turn.into(),
            },
            &mut cx,
        );
        if turn == "turn-1" {
            state.on_event(
                &id,
                AgentEvent::TurnCompleted {
                    turn_id: turn.into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
                &mut cx,
            );
        }
    }
    state.on_event(
        &id,
        AgentEvent::UserInputRequested {
            request_id: "ask".into(),
            questions: Vec::new(),
            delivery: agent::UserInputDelivery::Blocking,
        },
        &mut cx,
    );
    let status = state.session_status_snapshot(&id).unwrap();
    let started_at = state.resident(&id).unwrap().timeline.turns[1].start_ts;
    assert!(started_at.is_some());
    assert_eq!(
        status.running_turn,
        Some(tcode_core::session::RunningTurn {
            turn: 1,
            started_at
        })
    );
    assert_eq!(
        status.pending_user_input.map(|pending| pending.request_id),
        Some("ask".into())
    );

    // A background subagent keeps running, and asking, after the turn that
    // spawned it ends; the turn's own requests end with it.
    let approval = |id: &str, parent_item_id: Option<&str>| agent::ApprovalRequest {
        id: id.into(),
        turn_id: None,
        parent_item_id: parent_item_id.map(Into::into),
        kind: agent::ApprovalKind::ExecCommand {
            command: "cp a b".into(),
            cwd: None,
            description: None,
        },
        reason: None,
        blocked_path: None,
        options: Vec::new(),
    };
    for request in [
        approval("own", None),
        approval("subagent-during-turn", Some("toolu_agent")),
    ] {
        state.on_event(&id, AgentEvent::ApprovalRequested(request), &mut cx);
    }
    state.on_event(
        &id,
        AgentEvent::TurnCompleted {
            turn_id: "turn-2".into(),
            status: TurnStatus::Completed,
            usage: None,
        },
        &mut cx,
    );
    state.on_event(
        &id,
        AgentEvent::ApprovalRequested(approval("subagent-between-turns", Some("toolu_agent"))),
        &mut cx,
    );
    let pending = |state: &AppState| {
        state
            .session_status_snapshot(&id)
            .unwrap()
            .pending_approvals
            .into_iter()
            .map(|request| request.id)
            .collect::<Vec<_>>()
    };
    let status = state.session_status_snapshot(&id).unwrap();
    assert_eq!(status.running_turn, None);
    assert_eq!(status.pending_user_input, None);
    assert_eq!(
        pending(&state),
        ["subagent-during-turn", "subagent-between-turns"]
    );
    assert!(status.activity.waiting_for_approval);

    // Shut down without a closing record: the timeline still holds the turn
    // and its question.
    state.on_event(
        &id,
        AgentEvent::TurnStarted {
            turn_id: "turn-3".into(),
        },
        &mut cx,
    );
    state.on_event(
        &id,
        AgentEvent::UserInputRequested {
            request_id: "ask-again".into(),
            questions: Vec::new(),
            delivery: agent::UserInputDelivery::Blocking,
        },
        &mut cx,
    );
    state.resident_mut(&id).unwrap().shutdown_to_idle();
    assert!(state.resident(&id).unwrap().timeline.turn_running);
    let status = state.session_status_snapshot(&id).unwrap();
    assert_eq!(status.running_turn, None);
    assert_eq!(status.pending_user_input, None);
    assert!(status.pending_approvals.is_empty());
}

/// Times the initial snapshot plus the pages a client fetches while scrolling
/// to the top of a real long thread. Run with
/// `TCODE_HISTORY_BENCH_LOG=/path/to/thread.jsonl cargo test -p tcode-runtime
/// --release -- --ignored --nocapture history_paging_bench`.
#[test]
#[ignore = "manual benchmark: needs TCODE_HISTORY_BENCH_LOG pointing at a large JSONL log"]
fn history_paging_bench() {
    let source = std::env::var_os("TCODE_HISTORY_BENCH_LOG").expect("TCODE_HISTORY_BENCH_LOG");
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-history-paging-bench");
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, store.root().clone(), None);
    meta.id = "bench".into();
    store.upsert_meta(&meta).unwrap();
    store
        .apply(&[tcode_services::store::Mutation::replace_event_log(
            "bench",
            fs::read(&source).unwrap(),
        )])
        .unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let started = Instant::now();
    state.update(cx, |state, cx| state.select_session("bench", cx));
    let opened = started.elapsed();
    let (snapshot_elapsed, pages) = state.update(cx, |state, _| {
        let started = Instant::now();
        let snapshot = events_reply(
            state,
            &tcode_protocol::Subscription {
                known_index: None,
                topic: Topic::SessionEvents {
                    session_id: "bench".into(),
                },
                after: None,
            },
        );
        let snapshot_elapsed = started.elapsed();
        let ServerEvent::SessionSnapshot { from, total, .. } = snapshot.event else {
            panic!("snapshot")
        };
        eprintln!("snapshot: {snapshot_elapsed:?} (from {from} of {total} records)");
        let mut before = from;
        let mut pages = Vec::new();
        while before > 0 && pages.len() < 16 {
            let started = Instant::now();
            let QueryResponse::SessionHistoryPage { from, records, .. } =
                state.event_records["bench"]
                    .history_page(before, tcode_protocol::SESSION_HISTORY_RECORDS as u32)
                    .unwrap()
            else {
                panic!("page")
            };
            pages.push((started.elapsed(), records.len()));
            before = from;
        }
        (snapshot_elapsed, pages)
    });
    cx.run_until_parked();
    let paged: Duration = pages.iter().map(|(elapsed, _)| *elapsed).sum();
    for (index, (elapsed, records)) in pages.iter().enumerate() {
        eprintln!("page {index}: {elapsed:?} ({records} records)");
    }
    eprintln!(
        "select_session: {opened:?}; snapshot: {snapshot_elapsed:?}; {} pages: {paged:?}; snapshot + pages: {:?}",
        pages.len(),
        snapshot_elapsed + paged
    );
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

fn tool_call(id: &str, output: String) -> SessionEventRecord {
    SessionEventRecord {
        origin: None,
        author: None,
        ts: Some(1),
        event: AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::ToolCall {
                image_reads: Vec::new(),
                name: "screenshot".into(),
                input: serde_json::json!({}),
                output: Some(output),
                status: ItemStatus::Completed,
            },
        }),
        elided: None,
    }
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
        SessionEventRecord {
            origin: None,
            author: None,
            ts: Some(2),
            event: AgentEvent::ItemCompleted(ThreadItem {
                id: "command".into(),
                parent_item_id: None,
                content: ItemContent::CommandExecution {
                    command: "make".into(),
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
        .unwrap()
    };
    let update = |diff: &str| SessionEventRecord {
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
        SessionEventRecord {
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
            state.event_records["changes"].history_page(4, 200).unwrap()
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

/// A stored turn-changes snapshot: the whole diff of `turn` so far.
fn stored_snapshot(ts: u64, turn: &str, diff: &str) -> String {
    format!(
        "{{\"ts\":{ts},\"event\":{{\"type\":\"turn_changes_updated\",\"turn_id\":\"{turn}\",\
         \"changes\":[{{\"path\":\"f\",\"kind\":\"modify\",\"diff\":{}}}],\
         \"completeness\":\"exact\"}}}}\n",
        serde_json::to_string(&Some(diff).filter(|diff| !diff.is_empty())).unwrap()
    )
}

/// A snapshot appended to an open thread drops the diffs of the one it
/// supersedes in the same commit, at that one's stored row even where blank or
/// undecodable rows put rows and records out of step, and the log windows are
/// served from holds it the same way. A log with a row that does not decode keeps its stored
/// rows: its fold may not name the snapshots another build's would.
#[test]
fn an_appended_snapshot_drops_the_superseded_ones_diffs_in_the_same_commit() {
    for (label, gap, rewritten) in [
        ("blank", "\n", true),
        ("undecodable", "{not valid json}\n", false),
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("append-supersedes");
        let seeded = format!(
            "{{\"ts\":1,\"event\":{{\"type\":\"turn_started\",\"turn_id\":\"t\"}}}}\n\n{}{gap}",
            stored_snapshot(2, "t", "-a\n+b\n")
        );
        store
            .apply(&[tcode_services::store::Mutation::replace_event_log(
                "thread",
                seeded.clone().into_bytes(),
            )])
            .unwrap();
        open_stored_session(&store, "thread");
        let mut conversation = store.read_events("thread").unwrap();
        let state = cx.new_entity(TestClientState::new((*store).clone()));
        state.update(cx, |state, cx| state.start_activity(cx));
        cx.run_until(|state| state.store.turn_index("thread").unwrap().is_some());
        park_running(&state, cx, "thread");
        window_reply(
            &through_pipe(&state, cx, 1, subscribe_events("thread", None)),
            1,
        );
        // The window can be served before the whole log is read and kept.
        cx.run_until(|state| state.event_records.contains_key("thread"));
        let snapshot = |diff: &str| AgentEvent::TurnChangesUpdated {
            turn_id: "t".into(),
            changes: vec![agent::FileChange {
                path: "f".into(),
                kind: agent::FileChangeKind::Modify,
                diff: Some(diff.into()),
            }],
            completeness: agent::ChangeCompleteness::Exact,
        };
        let window = state.update(cx, |state, cx| {
            for (ts, diff) in [(3, "-a\n+c\n"), (4, "-a\n+d\n")] {
                state.record_event_for_replica_test("thread", ts, &snapshot(diff), cx);
                conversation.push(SessionEventRecord {
                    origin: None,
                    author: None,
                    ts: Some(ts),
                    event: snapshot(diff),
                    elided: None,
                });
            }
            let ServerEvent::SessionSnapshot { records, .. } = events_reply(
                state,
                &tcode_protocol::Subscription {
                    known_index: None,
                    topic: Topic::SessionEvents {
                        session_id: "thread".into(),
                    },
                    after: None,
                },
            )
            .event
            else {
                panic!("snapshot")
            };
            records
        });
        let diffs: Vec<_> = window
            .iter()
            .filter_map(|record| match &record.event {
                AgentEvent::TurnChangesUpdated { changes, .. } => Some(changes[0].diff.is_some()),
                _ => None,
            })
            .collect();
        assert_eq!(diffs, [false, false, true], "{label}: the served log");
        assert_eq!(
            Timeline::fold_stored(&window),
            Timeline::fold_stored(&conversation),
            "{label}"
        );
        cx.run_until_parked();

        let stored = String::from_utf8(store.read_event_log("thread").unwrap()).unwrap();
        let expected = if rewritten {
            format!(
                "{{\"ts\":1,\"event\":{{\"type\":\"turn_started\",\"turn_id\":\"t\"}}}}\n\n{}{gap}{}{}",
                stored_snapshot(2, "t", ""),
                stored_snapshot(3, "t", ""),
                stored_snapshot(4, "t", "-a\n+d\n"),
            )
        } else {
            format!(
                "{seeded}{}{}",
                stored_snapshot(3, "t", "-a\n+c\n"),
                stored_snapshot(4, "t", "-a\n+d\n"),
            )
        };
        assert_eq!(stored, expected, "{label}: the stored rows");
        assert_eq!(
            Timeline::fold_events(store.read_events("thread").unwrap()),
            Timeline::fold_stored(&conversation),
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
    let logged = captured_log().len();
    let cx = &mut TestAppContext::default();
    let restarted = cx.new_entity(TestClientState::new((*store).clone()));
    restarted.update(cx, |state, cx| state.start_storage_pass(cx));
    cx.run_until(|state| state.storage_pass.is_none());
    assert!(
        captured_log()[logged..]
            .iter()
            .any(|line| line.text == "storage pass complete: no work")
    );
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

/// A completion appended to an open thread drops the deltas it repeats in
/// the same commit: the one that created the entry stays with its text
/// emptied, a stream the completion only abbreviates stays whole, and every
/// later append still takes the row after the last. A thread nobody holds
/// keeps its deltas until the background pass, which leaves the same rows.
/// A client that holds the log up to a dropped delta and reads on from the
/// store folds the same thread.
#[test]
fn a_completion_drops_the_deltas_it_repeats_in_the_same_commit() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("completion-supersedes");
    let seeded = "{\"ts\":1,\"event\":{\"type\":\"turn_started\",\"turn_id\":\"t\"}}\n";
    for id in ["held", "cold"] {
        store
            .apply(&[tcode_services::store::Mutation::replace_event_log(
                id,
                seeded.as_bytes().to_vec(),
            )])
            .unwrap();
        open_stored_session(&store, id);
    }
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    park_running(&state, cx, "held");
    window_reply(
        &through_pipe(&state, cx, 1, subscribe_events("held", None)),
        1,
    );
    cx.run_until(|state| state.event_records.contains_key("held"));
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
    let command = |output: &str| ItemContent::CommandExecution {
        command: "ls".into(),
        output: output.into(),
        exit_code: Some(0),
        status: agent::ItemStatus::Completed,
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
        completed("c", command("xxxx… 36 bytes omitted")),
        AgentEvent::TurnCompleted {
            turn_id: "t".into(),
            status: TurnStatus::Completed,
            usage: None,
        },
    ];
    state.update(cx, |state, cx| {
        for id in ["held", "cold"] {
            for (ts, event) in events.iter().enumerate() {
                state.record_event_for_replica_test(id, ts as u64 + 2, event, cx);
            }
        }
    });
    cx.run_until_parked();
    let all: Vec<String> = std::iter::once(seeded.trim_end().to_owned())
        .chain(events.iter().enumerate().map(|(ts, event)| {
            let mut row = serde_json::json!({"ts": ts + 2, "event": event});
            if matches!(event, AgentEvent::ItemCompleted(_)) {
                row["item_version"] = 1.into();
            }
            serde_json::to_string(&row).unwrap()
        }))
        .collect();
    let held = String::from_utf8(store.read_event_log("held").unwrap()).unwrap();
    let expected: String = [
        all[0].clone(),
        all[1].replace("\"text\":\"Hel\"", "\"text\":\"\""),
        all[3].clone(),
        all[4].clone(),
        all[5].clone(),
        all[6].clone(),
    ]
    .iter()
    .map(|line| format!("{line}\n"))
    .collect();
    assert_eq!(held, expected);
    let log = store.read_log("held").unwrap();
    assert_eq!(log.rows, [0, 1, 3, 4, 5, 6]);
    let full = store.read_log("cold").unwrap();
    assert_eq!(
        full.rows,
        [0, 1, 2, 3, 4, 5, 6],
        "a thread nobody holds keeps its deltas"
    );
    assert_eq!(
        Timeline::fold_stored(&log.records),
        Timeline::fold_stored(&full.records)
    );
    // A client holding rows 0..=2 (the second delta among them) reads on.
    let mut resumed = full.records[..3].to_vec();
    resumed.extend(store.read_rows("held", 3..u64::MAX).unwrap().records);
    assert_eq!(
        Timeline::fold_stored(&resumed),
        Timeline::fold_stored(&full.records)
    );
    state.update(cx, |state, cx| {
        state.record_event_for_replica_test("held", 20, &events[5], cx)
    });
    cx.run_until_parked();
    assert_eq!(store.read_log("held").unwrap().rows.last(), Some(&7));

    assert!(
        store
            .threads_to_compact()
            .unwrap()
            .contains(&"cold".to_owned())
    );
    state.update(cx, |state, cx| state.start_storage_pass(cx));
    cx.run_until(|state| state.storage_pass.is_none());
    assert_eq!(
        String::from_utf8(store.read_event_log("cold").unwrap()).unwrap(),
        expected
    );
    assert!(store.threads_to_compact().unwrap().is_empty());
}

/// A thread opens with about half a megabyte of history however long its
/// records are, and a single record larger than that still arrives alone.
#[test]
fn history_windows_are_byte_budgeted() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-window-bytes");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let records: Vec<SessionEventRecord> = (0..200)
        .map(|index| SessionEventRecord {
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
        state.event_records.insert("wide".into(), session_log(huge));
        let QueryResponse::SessionHistoryPage {
            from,
            records: sent,
            ..
        } = state.event_records["wide"].history_page(200, 200).unwrap()
        else {
            panic!("page")
        };
        assert_eq!((from, sent.len()), (199, 1));
    });
}

const CRASH_CHILD_DIR: &str = "TCODE_CRASH_CHILD_DIR";
const CRASH_ACK: &str = "crash child acked: ";

/// The process [`store_writer_survives_sigkill`] kills: it streams numbered
/// events through the real store writer and prints the last index every
/// flush barrier confirmed, until it is killed.
#[test]
#[ignore = "the child process of store_writer_survives_sigkill, which runs it"]
fn store_writer_crash_child() {
    use std::io::Write as _;
    let root = PathBuf::from(std::env::var_os(CRASH_CHILD_DIR).unwrap());
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new(SessionStore::open_at(root).unwrap()));
    let (commands, _receiver) = smol::channel::unbounded();
    let parked = live_session(ProviderKind::Codex, commands);
    let id = parked.meta.id.clone();
    state.update(cx, |state, _| {
        state.residents.parked.insert(id.clone(), parked);
    });
    println!("{CRASH_ACK}session {id}");
    // About 2 KiB a record, so the WAL passes the 1000-page auto-checkpoint
    // within the first few thousand records.
    let padding = "x".repeat(2048);
    let mut next = 0_u64;
    loop {
        let barrier = state.update(cx, |state, cx| {
            for _ in 0..1 + next % 97 {
                state.record_event(
                    &id,
                    &persisted_assistant_event(&format!("{next} {padding}")),
                    cx,
                );
                next += 1;
            }
            state.store_write_barrier(cx)
        });
        cx.drain_outgoing();
        barrier.recv_blocking().unwrap().unwrap();
        println!("{CRASH_ACK}{}", next - 1);
        std::io::stdout().flush().unwrap();
    }
}

/// Kill a process mid-stream through the store writer, over and over: every
/// record a flush acknowledged is still there after a restart, the records
/// form a gapless prefix with no torn row, and the files pass sqlite3's
/// integrity check. `TCODE_CRASH_RUNS` sets the number of kills.
#[test]
#[ignore = "spawns and SIGKILLs child processes and needs the sqlite3 CLI; run deliberately"]
fn store_writer_survives_sigkill() {
    use std::io::BufRead as _;
    let runs: usize = std::env::var("TCODE_CRASH_RUNS")
        .ok()
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(50);
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;
    let mut random = move |range: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % range
    };
    let (mut checkpointed_runs, mut acknowledged, mut unacknowledged) = (0, 0, 0);
    for run in 0..runs {
        let root = std::env::temp_dir().join(format!("tcode-crash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut child = tcode_services::process::command(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::tests::store_writer_crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env(CRASH_CHILD_DIR, &root)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let (lines, received) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if let Some((_, report)) = line.split_once(CRASH_ACK) {
                    let _ = lines.send(report.to_owned());
                }
            }
        });
        let session = received
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
            .strip_prefix("session ")
            .unwrap()
            .to_owned();
        std::thread::sleep(Duration::from_millis(200 + random(2_800)));
        child.kill().unwrap();
        child.wait().unwrap();
        reader.join().unwrap();
        let acked = received
            .try_iter()
            .filter_map(|report| report.parse::<u64>().ok())
            .max();

        // Preserve the files exactly as the kill left them, before anything
        // opens them again.
        let preserved = root.join("preserved");
        std::fs::create_dir_all(&preserved).unwrap();
        for name in ["tcode.db", "tcode.db-wal"] {
            if root.join(name).exists() {
                std::fs::copy(root.join(name), preserved.join(name)).unwrap();
            }
        }
        let main_file = std::fs::metadata(root.join("tcode.db")).unwrap().len();
        // A fresh database is a handful of pages; anything larger was
        // written there by a checkpoint while records streamed.
        if main_file > 64 * 1024 {
            checkpointed_runs += 1;
        }

        let store = SessionStore::open_at(root.clone()).unwrap();
        let log = store.read_event_log(&session).unwrap();
        let rows = log
            .split_inclusive(|byte| *byte == b'\n')
            .collect::<Vec<_>>();
        assert!(
            rows.iter().all(|row| row.ends_with(b"\n")),
            "run {run}: torn row"
        );
        let events = store.read_events(&session).unwrap();
        assert_eq!(events.len(), rows.len(), "run {run}: a row does not parse");
        let indices: Vec<u64> = events
            .iter()
            .map(|stored| match &stored.event {
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::AssistantMessage { text, .. },
                    ..
                }) => text.split(' ').next().unwrap().parse().unwrap(),
                other => panic!("run {run}: unexpected record {other:?}"),
            })
            .collect();
        let expected: Vec<u64> = (0..indices.len() as u64).collect();
        assert_eq!(
            indices, expected,
            "run {run}: records are not a gapless prefix"
        );
        if let Some(acked) = acked {
            assert!(
                indices.len() as u64 > acked,
                "run {run}: acknowledged record {acked} lost ({} survived)",
                indices.len()
            );
            acknowledged += acked + 1;
            unacknowledged += indices.len() as u64 - (acked + 1);
        }
        store.close().unwrap();
        drop(store);

        let check = tcode_services::process::command("sqlite3")
            .arg(preserved.join("tcode.db"))
            .arg("PRAGMA integrity_check;")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&check.stdout).trim(),
            "ok",
            "run {run}: sqlite3 integrity_check: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        println!(
            "run {run}: acked {acked:?}, survived {}, main file {main_file} bytes",
            indices.len()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
    println!(
        "{runs} kills: {acknowledged} acknowledged records all survived, {unacknowledged} \
         committed but unacknowledged, {checkpointed_runs} runs crossed an auto-checkpoint"
    );
    assert!(checkpointed_runs > 0, "no run crossed an auto-checkpoint");
}

/// A streamed delta that waits for company still reaches the database with
/// nothing written after it, and one still waiting when the host closes the
/// queue is committed before the writer stops.
#[test]
fn store_writer_commits_a_waiting_delta_on_its_own_and_at_shutdown() {
    let store = TestStore::new("tcode-delta-window");
    let (failures, _failures) = smol::channel::unbounded();
    let writer = StoreWriter::new(
        (*store).clone(),
        SettingsStore::new(store.root().clone()),
        failures,
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
    let deadline = Instant::now() + Duration::from_secs(30);
    while texts().is_empty() {
        assert!(
            Instant::now() < deadline,
            "a lone delta was never committed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    writes.send_blocking(delta(" second")).unwrap();
    drop(writes);
    writer.join().unwrap();
    assert_eq!(texts(), ["first", " second"]);
}

/// A log as a read of densely numbered rows would load it.
fn session_log(records: impl IntoIterator<Item = SessionEventRecord>) -> SessionLog {
    let records: Vec<_> = records.into_iter().collect();
    let rows = (0..records.len() as u64).collect();
    SessionLog::new(tcode_services::store::EventLog {
        next_row: records.len() as u64,
        records,
        rows,
        undecodable: 0,
    })
}

/// Threads of [`storage_pass_survives_sigkill`]'s fixture, each a run of turns
/// whose every turn-changes snapshot carries the turn's whole, growing diff.
const CRASH_PASS_THREADS: usize = 6;

/// The process [`storage_pass_survives_sigkill`] kills: a host started with the
/// startup diff pass over the fixture in [`CRASH_CHILD_DIR`].
#[test]
#[ignore = "the child process of storage_pass_survives_sigkill, which runs it"]
fn storage_pass_crash_child() {
    let root = PathBuf::from(std::env::var_os(CRASH_CHILD_DIR).unwrap());
    let _host = crate::pipe::spawn_host(
        SessionStore::open_at(root).unwrap(),
        crate::pipe::HostServices::default(),
    )
    .unwrap();
    println!("{CRASH_ACK}started");
    loop {
        std::thread::park();
    }
}

/// Kill a host while its startup pass drops superseded diffs, over and over:
/// every thread is afterwards either exactly as it was and still to be passed,
/// or exactly as a completed pass leaves it and marked passed, never a mix,
/// and the files pass sqlite3's integrity check. `TCODE_CRASH_RUNS` sets the
/// number of kills.
#[test]
#[ignore = "spawns and SIGKILLs child processes and needs the sqlite3 CLI; run deliberately"]
fn storage_pass_survives_sigkill() {
    let runs: usize = std::env::var("TCODE_CRASH_RUNS")
        .ok()
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(30);
    let scratch = std::env::temp_dir().join(format!("tcode-pass-crash-{}", uuid::Uuid::new_v4()));
    let template = scratch.join("template");
    let ids: Vec<String> = (0..CRASH_PASS_THREADS)
        .map(|thread| format!("thread-{thread}"))
        .collect();
    {
        let store = SessionStore::open_at(template.clone()).unwrap();
        for id in &ids {
            let mut log = String::new();
            for turn in 0..10 {
                log.push_str(&format!(
                    "{{\"ts\":{turn},\"event\":{{\"type\":\"turn_started\",\"turn_id\":\"{turn}\"}}}}\n"
                ));
                let mut diff = String::new();
                for edit in 0..40 {
                    diff.push_str(&format!("+line {edit} {}\n", "x".repeat(1_000)));
                    log.push_str(&stored_snapshot(edit, &turn.to_string(), &diff));
                }
            }
            let mut meta = SessionMeta::new(ProviderKind::Codex, template.clone(), None);
            meta.id = id.clone();
            store
                .apply(&[
                    tcode_services::store::Mutation::upsert_meta(meta),
                    tcode_services::store::Mutation::replace_event_log(id, log.into_bytes()),
                ])
                .unwrap();
        }
        store.close().unwrap();
    }
    let copy = |to: &Path| {
        std::fs::create_dir_all(to).unwrap();
        std::fs::copy(template.join("tcode.db"), to.join("tcode.db")).unwrap();
    };
    let rows_of = |store: &SessionStore, id: &str| store.read_event_log(id).unwrap();
    let passed_dir = scratch.join("passed");
    copy(&passed_dir);
    let (before, after, pass_time) = {
        let store = SessionStore::open_at(passed_dir.clone()).unwrap();
        let before: Vec<_> = ids.iter().map(|id| rows_of(&store, id)).collect();
        let started = Instant::now();
        for id in &ids {
            let mut plan = store.plan_compaction(id).unwrap();
            while store.compact(&mut plan).unwrap().is_none() {}
            assert!(matches!(
                store.compact(&mut plan).unwrap(),
                Some(tcode_services::store::Compaction::Compacted { .. })
            ));
        }
        let pass_time = started.elapsed();
        let after: Vec<_> = ids.iter().map(|id| rows_of(&store, id)).collect();
        store.close().unwrap();
        (before, after, pass_time)
    };
    println!(
        "fixture: {} bytes in {CRASH_PASS_THREADS} threads, passed in {pass_time:.1?} in process",
        before.iter().map(Vec::len).sum::<usize>()
    );
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;
    let mut random = move |range: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % range
    };
    let (mut mixed_runs, mut partial_runs) = (0, 0);
    for run in 0..runs {
        let root = scratch.join(format!("run-{run}"));
        copy(&root);
        let mut child = tcode_services::process::command(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::tests::storage_pass_crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env(CRASH_CHILD_DIR, &root)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert_ne!(
                std::io::BufRead::read_line(&mut stdout, &mut line).unwrap(),
                0,
                "run {run}: the child exited before starting"
            );
            if line.contains(CRASH_ACK) {
                break;
            }
        }
        let delay = random(pass_time.as_millis() as u64 * 3 / 2 + 1);
        std::thread::sleep(Duration::from_millis(delay));
        child.kill().unwrap();
        child.wait().unwrap();

        let preserved = root.join("preserved");
        std::fs::create_dir_all(&preserved).unwrap();
        for name in ["tcode.db", "tcode.db-wal"] {
            if root.join(name).exists() {
                std::fs::copy(root.join(name), preserved.join(name)).unwrap();
            }
        }
        let store = SessionStore::open_at(root.clone()).unwrap();
        let unpassed = store.threads_to_compact().unwrap();
        let mut done = 0;
        for (index, id) in ids.iter().enumerate() {
            let rows = rows_of(&store, id);
            if unpassed.contains(id) {
                assert!(
                    rows == before[index],
                    "run {run}: {id} changed but is unpassed"
                );
            } else {
                assert!(
                    rows == after[index],
                    "run {run}: {id} is passed but not as a pass leaves it"
                );
                done += 1;
            }
        }
        if done > 0 && done < ids.len() {
            partial_runs += 1;
        }
        store.close().unwrap();
        drop(store);
        let check = tcode_services::process::command("sqlite3")
            .arg(preserved.join("tcode.db"))
            .arg("PRAGMA integrity_check;")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&check.stdout).trim(),
            "ok",
            "run {run}: sqlite3 integrity_check: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        mixed_runs += usize::from(done > 0);
        println!(
            "run {run}: killed after {delay} ms, {done} of {CRASH_PASS_THREADS} threads passed"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
    std::fs::remove_dir_all(&scratch).unwrap();
    println!("{runs} kills: {mixed_runs} after some thread passed, {partial_runs} mid-pass");
    assert!(partial_runs > 0, "no kill landed mid-pass");
}

/// The reply a subscription to a session whose log is resident gets.
fn events_reply(state: &AppState, subscription: &Subscription) -> EventEnvelope {
    let Topic::SessionEvents { session_id } = &subscription.topic else {
        panic!("a subscription to a session's events")
    };
    EventEnvelope {
        request_id: None,
        topic: subscription.topic.clone(),
        event: state.event_records[session_id].events_window(subscription),
    }
}

/// Hand `payload` to the host as client message `id`, without running
/// anything it queues.
fn send_to_host(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    payload: tcode_protocol::ClientPayload,
) {
    state.deliver(cx, id, payload);
}

/// Run the host until it answered client message `id` and parked, and
/// return every message it serialized since the last drain.
fn answered(cx: &mut TestAppContext, id: u64) -> Vec<HostMessage> {
    cx.run_until_sent(|message| {
        matches!(message, HostMessage::Ack { id: reply, .. } | HostMessage::QueryResult { id: reply, .. } if *reply == id)
            .then_some(())
    })
    .1
}

/// Send `payload` through the host pipe as client message `id`, and return
/// every message the host serialized since the last drain, once the host
/// answered it and parked.
fn through_pipe(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    payload: tcode_protocol::ClientPayload,
) -> Vec<HostMessage> {
    send_to_host(state, cx, id, payload);
    answered(cx, id)
}

fn subscribe_events(session_id: &str, after: Option<u64>) -> tcode_protocol::ClientPayload {
    tcode_protocol::ClientPayload::Subscribe(Subscription {
        known_index: None,
        topic: Topic::SessionEvents {
            session_id: session_id.into(),
        },
        after,
    })
}

fn unsubscribe_events(session_id: &str) -> tcode_protocol::ClientPayload {
    tcode_protocol::ClientPayload::Unsubscribe(Subscription {
        known_index: None,
        topic: Topic::SessionEvents {
            session_id: session_id.into(),
        },
        after: None,
    })
}

fn history_page(session_id: &str, before: u64) -> tcode_protocol::ClientPayload {
    tcode_protocol::ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage {
        session_id: session_id.into(),
        before,
        limit: 200,
    })
}

/// A session window as the host sent it.
#[derive(Debug, Clone, PartialEq)]
struct Window {
    from: u64,
    end: u64,
    records: Vec<SessionEventRecord>,
    total: u64,
    total_turns: u64,
}

fn window_of(event: &ServerEvent) -> Option<Window> {
    match event {
        ServerEvent::SessionSnapshot {
            from,
            end,
            records,
            total,
            total_turns,
            ..
        } => Some(Window {
            from: *from,
            end: *end,
            records: records.clone(),
            total: *total,
            total_turns: *total_turns,
        }),
        _ => None,
    }
}

/// The window answering subscription request `id`.
fn window_reply(messages: &[HostMessage], id: u64) -> Window {
    messages
        .iter()
        .find_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                request_id: Some(request),
                event,
                ..
            }) if *request == id => window_of(event),
            _ => None,
        })
        .expect("a window answers the subscription")
}

fn query_reply(messages: &[HostMessage], id: u64) -> QueryResponse {
    messages
        .iter()
        .find_map(|message| match message {
            HostMessage::QueryResult { id: reply, result } if *reply == id => {
                Some(result.clone().expect("query succeeds"))
            }
            _ => None,
        })
        .expect("the query is answered")
}

fn open_stored_session(store: &SessionStore, id: &str) -> SessionMeta {
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, store.root().clone(), None);
    meta.id = id.into();
    store.upsert_meta(&meta).unwrap();
    meta
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
    let host = spawn_host((*root).clone(), HostServices::default()).unwrap();
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
    smol::block_on(host.update_state_for_test(|state, cx| {
        state.record_event(
            "cold",
            &AgentEvent::Warning {
                message: "first live".into(),
            },
            cx,
        );
    }))
    .unwrap();
    for link in &clients {
        let mut received = Vec::new();
        let events = link.events();
        smol::block_on(smol::future::race(
            async {
                let mut has_window = false;
                while received.len() < persisted.len() + 1 {
                    let event = events.recv().await.unwrap().event;
                    if let Some(window) = window_of(&event) {
                        assert!(!has_window, "one baseline per client");
                        assert_eq!(window.from, 0);
                        received.extend(window.records);
                        has_window = true;
                    } else if let ServerEvent::SessionEvent(record) = event {
                        assert!(has_window, "baseline precedes live records");
                        received.push(record);
                    } else {
                        panic!("unexpected {event:?}");
                    }
                }
                // A FIFO reply proves all preceding records have reached the pump.
                link.query(tcode_protocol::Query::Ping).await.unwrap();
                assert!(
                    events.try_recv().is_err(),
                    "no duplicate records after the tail"
                );
            },
            async {
                smol::Timer::after(Duration::from_secs(5)).await;
                panic!("the host stopped delivering the thread");
            },
        ));
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
/// window for it, and a log read for a session that is no longer resident is
/// not kept.
#[test]
fn a_cold_thread_left_before_its_log_is_read_sends_no_window_and_keeps_no_log() {
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
    state.read(|state| {
        assert!(state.resident("left").is_none());
        assert!(state.event_records.is_empty());
    });
}

/// History pages and whole outputs of a thread no client holds open are
/// answered from its whole log, read off the mailbox and not kept.
#[test]
fn pages_and_outputs_of_a_thread_nobody_opened_are_read_and_not_kept() {
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
    state.read(|state| {
        assert!(state.resident("cold").is_none());
        assert!(state.event_records.is_empty());
    });
}

/// A child whose turn completes while its log is still being read reports
/// that turn: its status and result, and the callback to its parent, see the
/// completion and the final message accepted just before it.
#[test]
fn a_child_completing_while_its_log_is_read_reports_that_turn() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("child-completing-while-read");
    let mut child = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
    child.id = "child".into();
    child.parent_session_id = Some("parent".into());
    child.expectation = Some(ChildExpectation::opened(1));
    store.upsert_meta(&child).unwrap();
    persist_streamed_turns(&store, "child", 1);
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);
        state.load_background_session(child, cx);
        for event in [
            AgentEvent::TurnStarted {
                turn_id: "turn-new".into(),
            },
            report_result_event("rep-new", "the new report"),
            persisted_assistant_event("the new answer"),
            AgentEvent::TurnCompleted {
                turn_id: "turn-new".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ] {
            state.on_event("child", event, cx);
        }
        assert!(state.log_hydrations.contains_key("child"));
    });

    cx.run_until(|_| !parent_receiver.is_empty());
    let callback = std::iter::from_fn(|| parent_receiver.try_recv().ok())
        .find_map(|command| match command {
            SessionCommand::Steer { text, .. } => Some(text),
            _ => None,
        })
        .expect("the parent is called back");
    assert!(callback.starts_with("[orchestrate] thread child"));
    assert!(callback.ends_with("\nthe new report"));
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
            include_bytes!("../../../services/src/store/fixtures/turn-index.jsonl").to_vec(),
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
        window_of(&whole.events_window(&Subscription {
            topic: Topic::SessionEvents {
                session_id: "legacy".into()
            },
            after: None,
            known_index: None,
        }))
        .unwrap()
    );
    state.read(|state| {
        assert!(state.event_records.contains_key("legacy"));
        assert!(!state.log_hydrations.contains_key("legacy"));
    });
    let page = query_reply(
        &through_pipe(&state, cx, 2, history_page("legacy", window.end)),
        2,
    );
    assert_eq!(page, whole.history_page(window.end, 200).unwrap());
    assert!(store.turn_index("legacy").unwrap().is_none());
}

#[test]
fn message_origins_survive_queue_acknowledgement_and_callback_steering() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("message-origin");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (commands, received) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::Codex, commands);
    session.meta.id = "thread".into();
    state.update(cx, |state, _| state.install_selected(session));
    state.dispatch_command(
        cx,
        1,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "human work".into(),
            attachment_paths: vec![],
        },
    );
    let SessionCommand::SendTurn { delivery_id, .. } = received.try_recv().unwrap() else {
        panic!("send")
    };
    state.update(cx, |state, cx| {
        state.on_event("thread", AgentEvent::TurnAccepted { delivery_id }, cx);
        state.on_event(
            "thread",
            AgentEvent::TurnStarted {
                turn_id: "run".into(),
            },
            cx,
        );
        state.deliver_orchestrate_callback_to_parent("thread", "agent completion".into(), cx);
        // A message the provider reports on its own, such as an answer it
        // steers into the running turn.
        state.on_event(
            "thread",
            AgentEvent::SteerRequested {
                request_id: "provider-reply".into(),
                text: "A".into(),
                attachments: vec![],
            },
            cx,
        );
    });
    assert!(matches!(
        received.try_recv().unwrap(),
        SessionCommand::Steer { .. }
    ));
    cx.run_until_parked();
    let records = store.read_events("thread").unwrap();
    let messages: Vec<_> = records
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
        .collect();
    assert_eq!(
        messages
            .iter()
            .map(|record| record.origin)
            .collect::<Vec<_>>(),
        [Some(MessageOrigin::Human), Some(MessageOrigin::Agent), None]
    );
    let fold = |record: &SessionEventRecord| {
        tcode_core::settlement::ThreadActivity::fold_stored([record], false)
    };
    assert_eq!(fold(messages[1]).last_human_message_at, None);
    assert_eq!(
        fold(messages[2]).last_human_message_at,
        messages[2].ts,
        "a top-level thread's own provider message is the human's"
    );
}

fn next_host_event(events: &async_channel::Receiver<EventEnvelope>) -> EventEnvelope {
    smol::block_on(smol::future::race(
        async { events.recv().await.unwrap() },
        async {
            smol::Timer::after(Duration::from_secs(5)).await;
            panic!("host did not publish the expected lifecycle change")
        },
    ))
}

#[test]
fn cold_lifecycle_migrates_legacy_settlement_and_ages_from_events_after_rename() {
    use crate::pipe::{HostServices, spawn_host};
    let store = TestStore::new("cold-settlement");
    let stamp = now_millis() - 10 * 86_400_000;
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
                        context_len: None,
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
    let host = spawn_host((*store).clone(), HostServices::default()).unwrap();
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
            assert!(meta.updated_at > stamp / 1000);
            break;
        }
    }
    link.shutdown_blocking().unwrap();
    host.to_host.close();
    host.stopped.recv_blocking().unwrap();
    let restarted = spawn_host(
        SessionStore::open_at(store.root().clone()).unwrap(),
        HostServices::default(),
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
            let find = |id: &str| snapshot.sessions.iter().find(|meta| meta.id == id).unwrap();
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

#[test]
fn first_index_snapshot_includes_persisted_child_runs_and_human_resume_status() {
    use tcode_core::pull_request::{
        Mergeability, PullRequestKey, PullRequestSnapshot, PullRequestSource, PullRequestState,
        link_pull_request,
    };
    use tcode_services::store::Mutation;

    let store = TestStore::new("startup-human-clock");
    let hour = 3_600_000;
    let created = now_millis() - 10 * hour;
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
        updated_at: merged_at.clone(),
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
                    context_len: None,
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

#[test]
fn manual_settlement_is_thread_local_cancels_automatic_queue_and_blocks_human_admission() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("manual-settlement");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (commands, received) = smol::channel::unbounded();
    let original_commands = commands.clone();
    state.update(cx, |state, _| {
        let mut parent = live_session(ProviderKind::Codex, commands);
        parent.meta.id = "parent".into();
        parent.meta.unsettled_at = Some(1);
        parent.push_or_merge_orchestrate_callback("queued completion".into());
        let wake = parent.push_queued("automatic wake".into(), vec![]);
        parent
            .queue
            .iter_mut()
            .find(|message| message.id == wake)
            .unwrap()
            .origin = MessageOrigin::Server;
        let mut child = ActiveSession::new(
            SessionMeta::new(ProviderKind::Codex, store.root().clone(), None),
            false,
            vec![],
        );
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        child.turn_in_flight = true;
        state
            .sessions
            .extend([parent.meta.clone(), child.meta.clone()]);
        state.residents.parked.insert("child".into(), child);
        state.install_selected(parent);
    });
    through_pipe(
        &state,
        cx,
        1,
        tcode_protocol::ClientPayload::Command(Command::ScheduleTurn {
            session_id: "parent".into(),
            text: "human later".into(),
            attachment_paths: vec![],
            fire_at_unix_secs: now_secs() + 3600,
        }),
    );
    let denied = through_pipe(
        &state,
        cx,
        2,
        tcode_protocol::ClientPayload::Command(Command::SettleSession {
            session_id: "parent".into(),
        }),
    );
    assert!(denied.iter().any(|message| matches!(
        message,
        HostMessage::Ack { id: 2, result: Err(error) } if error.code == "thread_busy"
    )));
    let human = state.read(|state| {
        state
            .resident("parent")
            .unwrap()
            .queue
            .iter()
            .find(|message| message.origin == MessageOrigin::Human)
            .unwrap()
            .id
    });
    through_pipe(
        &state,
        cx,
        3,
        tcode_protocol::ClientPayload::Command(Command::DropQueued {
            session_id: "parent".into(),
            id: human,
        }),
    );
    for (request, resolved) in [
        (
            AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval".into(),
                parent_item_id: None,
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
                questions: vec![],
                delivery: agent::UserInputDelivery::Blocking,
            },
            AgentEvent::UserInputResolved {
                request_id: "blocking".into(),
                answers: Default::default(),
            },
        ),
    ] {
        state.update(cx, |state, cx| state.on_event("parent", request, cx));
        let denied = through_pipe(
            &state,
            cx,
            20,
            tcode_protocol::ClientPayload::Command(Command::SettleSession {
                session_id: "parent".into(),
            }),
        );
        assert!(denied.iter().any(|message| matches!(
            message,
            HostMessage::Ack { id: 20, result: Err(error) } if error.code == "thread_busy"
        )));
        state.update(cx, |state, cx| state.on_event("parent", resolved, cx));
    }
    state.update(cx, |state, cx| {
        state.on_event(
            "parent",
            AgentEvent::UserInputRequested {
                request_id: "asynchronous".into(),
                questions: vec![],
                delivery: agent::UserInputDelivery::Async,
            },
            cx,
        )
    });
    let accepted = through_pipe(
        &state,
        cx,
        4,
        tcode_protocol::ClientPayload::Command(Command::SettleSession {
            session_id: "parent".into(),
        }),
    );
    assert!(accepted.iter().any(|message| matches!(
        message,
        HostMessage::Ack {
            id: 4,
            result: Ok(_)
        }
    )));
    state.read(|state| {
        let parent = state.resident("parent").unwrap();
        assert!(parent.meta.is_settled());
        assert!(parent.meta.unsettled_at.is_none());
        assert!(parent.queue.is_empty());
        assert_eq!(state.active_session_id(), Some("parent"));
        assert!(state.resident("child").unwrap().turn_in_flight);
        assert!(!state.find_meta("child").unwrap().is_settled());
    });
    assert!(matches!(
        received.try_recv().unwrap(),
        SessionCommand::Shutdown
    ));
    through_pipe(
        &state,
        cx,
        21,
        tcode_protocol::ClientPayload::Subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionEvents {
                session_id: "parent".into(),
            },
            after: None,
        }),
    );
    state.read(|state| {
        assert!(
            state.find_meta("parent").unwrap().is_settled(),
            "reading leaves settlement intact"
        )
    });
    for command in [
        Command::ArchiveSession {
            session_id: "parent".into(),
        },
        Command::UnarchiveSession {
            session_id: "parent".into(),
        },
    ] {
        through_pipe(
            &state,
            cx,
            22,
            tcode_protocol::ClientPayload::Command(command),
        );
        state.read(|state| assert!(state.find_meta("parent").unwrap().is_settled()));
    }
    through_pipe(
        &state,
        cx,
        24,
        tcode_protocol::ClientPayload::Subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionStatus {
                session_id: "parent".into(),
            },
            after: None,
        }),
    );
    cx.run_until(|state| state.resident("parent").is_some());
    state.read(|state| {
        assert!(
            state.find_meta("parent").unwrap().is_settled(),
            "reopening status preserves settlement"
        )
    });
    through_pipe(
        &state,
        cx,
        23,
        tcode_protocol::ClientPayload::Command(Command::UnsettleSession {
            session_id: "parent".into(),
        }),
    );
    state.read(|state| {
        assert_eq!(
            state.find_meta("parent").unwrap().settled_override,
            Some(tcode_core::project::SettledOverride::Active)
        )
    });
    let fresh = smol::channel::unbounded();
    state.update(cx, |state, _| {
        state.resident_mut("parent").unwrap().runtime = Runtime::Live(fresh.0)
    });
    through_pipe(
        &state,
        cx,
        5,
        tcode_protocol::ClientPayload::Command(Command::SendTurn {
            session_id: "parent".into(),
            text: "new work".into(),
            attachment_paths: vec![],
        }),
    );
    assert!(
        matches!(fresh.1.try_recv().unwrap(), SessionCommand::SendTurn { text, .. } if text == "new work")
    );
    state.update(cx, |state, cx| {
        state.on_event_stream_ended("parent", &original_commands, cx)
    });
    state.read(|state| {
        let meta = state.find_meta("parent").unwrap();
        assert!(!meta.is_settled());
        assert_eq!(
            meta.settled_override, None,
            "an accepted message clears explicit Active"
        );
        assert!(meta.unsettled_at.is_some());
        assert!(
            state
                .resident("parent")
                .unwrap()
                .delivery_in_flight
                .is_some()
        );
    });
    through_pipe(
        &state,
        cx,
        6,
        tcode_protocol::ClientPayload::Command(Command::ShutdownAllAndFlush),
    );
    let restarted = AppState::new((*store).clone()).unwrap();
    assert!(!restarted.find_meta("parent").unwrap().is_settled());
}

#[test]
fn a_report_reopens_only_its_lead_and_never_reaches_an_archived_or_deleted_one() {
    for case in [
        "valid",
        "renamed",
        "archived_child",
        "archived_parent",
        "deleted_parent",
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("completion-admission");
        let state = cx.new_entity(TestClientState::new((*store).clone()));
        let (parent_commands, parent_received) = smol::channel::unbounded();
        let (child_commands, _child_received) = smol::channel::unbounded();
        state.update(cx, |state, _| {
            let mut ancestor = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
            ancestor.id = "ancestor".into();
            ancestor.settled_at = Some(1);
            let mut parent = live_session(ProviderKind::Codex, parent_commands);
            parent.meta.id = "parent".into();
            parent.meta.parent_session_id = Some("ancestor".into());
            parent.meta.settled_at = Some(1);
            let mut child = live_session(ProviderKind::Codex, child_commands);
            child.meta.id = "child".into();
            child.meta.parent_session_id = Some("parent".into());
            child.meta.expectation = Some(ChildExpectation::opened(1));
            state
                .sessions
                .extend([ancestor, parent.meta.clone(), child.meta.clone()]);
            state.residents.parked.insert("parent".into(), parent);
            state.install_selected(child);
        });
        // Each command lands before the child runs and reports.
        let command = match case {
            "renamed" => Some(Command::RenameSession {
                session_id: "child".into(),
                title: "Renamed".into(),
            }),
            "archived_child" => Some(Command::ArchiveSession {
                session_id: "child".into(),
            }),
            "archived_parent" => Some(Command::ArchiveSession {
                session_id: "parent".into(),
            }),
            "deleted_parent" => Some(Command::DeleteSession {
                session_id: "parent".into(),
                remove_worktree: false,
            }),
            _ => None,
        };
        if let Some(command) = command {
            state.dispatch_command(cx, 1, command);
        }
        cx.run_until_parked();
        state.update(cx, |state, cx| {
            if state.resident("child").is_none() {
                return;
            }
            for event in [
                AgentEvent::TurnStarted {
                    turn_id: "finished-run".into(),
                },
                report_result_event("rep", "completion evidence"),
                completed_turn("finished-run"),
            ] {
                state.on_event("child", event, cx);
            }
        });
        if case == "valid" || case == "renamed" {
            cx.run_until(|_| !parent_received.is_empty());
            assert!(
                matches!(parent_received.try_recv().unwrap(), SessionCommand::SendTurn { text, .. } if text.contains("completion evidence"))
            );
            state.read(|state| assert!(!state.find_meta("parent").unwrap().is_settled()));
        } else {
            cx.run_until_parked();
            state.read(|state| {
                if let Some(parent) = state.find_meta("parent") {
                    assert!(parent.is_settled(), "{case}");
                }
            });
            assert!(
                !std::iter::from_fn(|| parent_received.try_recv().ok()).any(|command| matches!(
                    command,
                    SessionCommand::SendTurn { .. } | SessionCommand::Steer { .. }
                )),
                "{case}"
            );
        }
        state.read(|state| assert!(state.find_meta("ancestor").unwrap().is_settled(), "{case}"));
    }
}

#[test]
fn manual_settle_waits_for_the_harness_background_work() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("settle-background-work");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (commands, received) = smol::channel::unbounded();
    state.update(cx, |state, cx| {
        let mut session = live_session(ProviderKind::ClaudeCode, commands);
        session.meta.id = "shell".into();
        state.sessions.push(session.meta.clone());
        state.install_selected(session);
        state.on_event("shell", AgentEvent::BackgroundTasksChanged { count: 1 }, cx);
    });
    let settle = || {
        tcode_protocol::ClientPayload::Command(Command::SettleSession {
            session_id: "shell".into(),
        })
    };
    let denied = through_pipe(&state, cx, 1, settle());
    assert!(denied.iter().any(|message| matches!(
        message,
        HostMessage::Ack { id: 1, result: Err(error) } if error.code == "thread_busy"
    )));
    state.read(|state| assert!(!state.find_meta("shell").unwrap().is_settled()));
    assert!(
        received.try_recv().is_err(),
        "a refused settle leaves the harness and its background work running"
    );

    state.update(cx, |state, cx| {
        state.on_event("shell", AgentEvent::BackgroundTasksChanged { count: 0 }, cx)
    });
    let accepted = through_pipe(&state, cx, 2, settle());
    assert!(accepted.iter().any(|message| matches!(
        message,
        HostMessage::Ack {
            id: 2,
            result: Ok(_)
        }
    )));
    state.read(|state| assert!(state.find_meta("shell").unwrap().is_settled()));
}

#[test]
fn a_finished_child_holds_its_lead_until_settled_and_cancel_never_delivers() {
    use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};
    use tcode_protocol::AgentStatus;

    let cx = &mut TestAppContext::default();
    let store = TestStore::new("agent-delivery");
    let stamp = now_millis() - 10 * 86_400_000;
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
            state.handle_control_op("lead".into(), op, reply, cx)
        });
        cx.run_until(|_| !response.is_empty());
        response.try_recv().unwrap()
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
            request_id: None,
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
    let stamp = now_millis() - 60_000;
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

#[test]
fn settling_detaches_only_its_own_provider_and_releases_its_parent() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("settlement-own-provider");
    let stamp = now_millis() - 10 * 86_400_000;
    let mut lead = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
    lead.id = "lead".into();
    lead.created_at = stamp / 1000;
    store.upsert_meta(&lead).unwrap();
    store
        .append_event(
            "lead",
            stamp,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "old-request".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "Earlier work".into(),
                    context_len: None,
                    attachments: vec![],
                },
            }),
        )
        .unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (worker_commands, worker_received) = smol::channel::unbounded();
    state.update(cx, |state, cx| {
        state.settings.auto_settle_after_days = None;
        let mut worker = live_session(ProviderKind::Codex, worker_commands);
        worker.meta.id = "worker".into();
        worker.meta.parent_session_id = Some("lead".into());
        let mut independent = ActiveSession::new(
            SessionMeta::new(ProviderKind::Codex, store.root().clone(), None),
            false,
            vec![],
        );
        independent.meta.id = "independent".into();
        independent.meta.parent_session_id = Some("worker".into());
        independent.turn_in_flight = true;
        state.persist_meta(&worker.meta, cx);
        state.persist_meta(&independent.meta, cx);
        state
            .residents
            .parked
            .insert("independent".into(), independent);
        state.install_selected(worker);
        for owner in ["worker", "independent"] {
            state.on_event(
                owner,
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
                }),
                cx,
            );
        }
    });
    // An unsettled dispatched child holds its lead out of settlement.
    state.dispatch_command(
        cx,
        1,
        Command::PatchSettings {
            patch: SettingsPatch::AutoSettleAfterDays(Some(3.0)),
        },
    );
    cx.run_until(|state| state.thread_activity.contains_key("lead"));
    state.read(|state| assert!(!state.find_meta("lead").unwrap().is_settled()));
    let accepted = through_pipe(
        &state,
        cx,
        2,
        tcode_protocol::ClientPayload::Command(Command::SettleSession {
            session_id: "worker".into(),
        }),
    );
    assert!(accepted.iter().any(|message| matches!(
        message,
        HostMessage::Ack {
            id: 2,
            result: Ok(_)
        }
    )));
    assert!(matches!(
        worker_received.try_recv().unwrap(),
        SessionCommand::Shutdown
    ));
    state.read(|state| {
        let mirror = |spawn| {
            state
                .sessions
                .iter()
                .find(|meta| meta.native_subagent.as_deref() == Some(spawn))
                .unwrap()
        };
        assert_eq!(
            state
                .resident(&mirror("spawn-worker").id)
                .unwrap()
                .timeline
                .last_turn_status,
            Some(TurnStatus::Interrupted)
        );
        assert!(
            state
                .resident(&mirror("spawn-independent").id)
                .unwrap()
                .timeline
                .turn_running
        );
        assert!(state.resident("independent").unwrap().turn_in_flight);
    });
    state.dispatch_command(
        cx,
        3,
        Command::PatchSettings {
            patch: SettingsPatch::AutoSettleAfterDays(Some(2.0)),
        },
    );
    cx.run_until(|state| {
        state
            .find_meta("lead")
            .is_some_and(|meta| meta.settled_at == Some(stamp / 1000))
    });
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
    let stamp = now_millis() - 10 * 86_400_000;
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
                        context_len: None,
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
            3,
            Command::ReorderActive {
                session_id: "idle".into(),
                order_key: "m".into(),
            },
        ),
        Err("not_active".into())
    );
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
        5,
        Command::ReorderActive {
            session_id: "arranged".into(),
            order_key: "m".into(),
        },
    )
    .unwrap();
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
        for id in ["idle", "arranged"] {
            let meta = state.find_meta(id).unwrap();
            assert_eq!(meta.updated_at, stamp / 1000, "arranging is not activity");
        }
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
        let arranged = state.find_meta("arranged").unwrap();
        assert_eq!(
            arranged.active_order, None,
            "settling clears the arrangement"
        );
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

/// Verifying a token saves it only when the server takes it: one the host cannot ask the
/// server about stays unsaved, and saving it anyway leaves it saved and unverified.
#[test]
fn a_token_the_server_cannot_be_asked_about_is_saved_only_when_saved_anyway() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-verify-host-token");
    let secrets = || -> serde_json::Value {
        std::fs::read(store.root().join("secrets.json"))
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .unwrap_or_default()
    };
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        state.forge = tcode_services::forge::connect(SettingsStore::new(store.root().clone()), []);
    });
    // Nothing listens on port 1, so the server answers nothing.
    let host = "127.0.0.1:1";
    state.dispatch_command(
        cx,
        701,
        Command::PatchSettings {
            patch: SettingsPatch::SourceControlHost {
                host: host.into(),
                kind: tcode_core::pull_request::HostKind::Gitlab,
                enabled: Some(true),
                account: None,
            },
        },
    );
    state.dispatch_command(
        cx,
        702,
        Command::VerifyHostToken {
            host: host.into(),
            token: "unasked-token".into(),
            email: None,
        },
    );
    let (result, _) = cx.run_until_sent(|message| match message {
        HostMessage::Ack { id: 702, result } => Some(result.clone()),
        _ => None,
    });
    assert_eq!(
        result,
        Ok(CommandResponse::TokenCheck(
            tcode_core::settings::TokenCheck::Unreachable { status: None }
        ))
    );
    assert!(secrets().get("@gitlab").is_none());
    state.dispatch_command(
        cx,
        703,
        Command::SetHostToken {
            host: host.into(),
            token: Some("unasked-token".into()),
            email: None,
        },
    );
    cx.run_until(|state| {
        state
            .settings_snapshot()
            .source_control
            .status
            .get(host)
            .is_some_and(|status| {
                status.problem == Some(tcode_core::settings::HostProblem::Unverified)
            })
    });
    assert_eq!(secrets()["@gitlab"][host], "unasked-token");
}

#[test]
fn github_secret_command_persists_separately_and_settings_never_replicate_it() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-github-secret-command");
    let settings_store = SettingsStore::new(store.root().clone());
    let persisted = || -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(store.root().join("secrets.json")).unwrap()).unwrap()
    };
    let mut legacy_settings = Settings::default();
    legacy_settings.profiles.insert(
        "github".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings::default(),
        },
    );
    settings_store.save(&legacy_settings).unwrap();
    std::fs::write(
        store.root().join("secrets.json"),
        r#"{"github":{"ANTHROPIC_API_KEY":"legacy-provider-secret"}}"#,
    )
    .unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        state.forge = tcode_services::forge::connect(SettingsStore::new(store.root().clone()), []);
    });
    state.dispatch_command(
        cx,
        638,
        Command::SetHostToken {
            host: "GITHUB.COM".into(),
            token: Some("github-writer-secret".into()),
            email: None,
        },
    );
    cx.run_until(|_| persisted()["@github"]["github.com"] == "github-writer-secret");
    // Older builds read secrets.json as profile maps and keep entries they do not know.
    serde_json::from_value::<
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    >(persisted())
    .unwrap();
    assert_eq!(
        settings_store.profile_secrets("github")["ANTHROPIC_API_KEY"],
        "legacy-provider-secret"
    );
    state.dispatch_command(
        cx,
        642,
        Command::SetProfileSecret {
            profile_id: "github".into(),
            name: "ANTHROPIC_API_KEY".into(),
            value: Some("provider-replacement-secret".into()),
        },
    );
    cx.run_until(|_| {
        settings_store
            .profile_secrets("github")
            .get("ANTHROPIC_API_KEY")
            .map(String::as_str)
            == Some("provider-replacement-secret")
    });
    assert_eq!(persisted()["@github"]["github.com"], "github-writer-secret");
    state.dispatch_command(
        cx,
        639,
        Command::PatchSettings {
            patch: SettingsPatch::SourceControlHost {
                host: "github.com".into(),
                kind: tcode_core::pull_request::HostKind::Github,
                enabled: Some(false),
                account: Some(Some("sample".into())),
            },
        },
    );
    state.dispatch_command(
        cx,
        640,
        Command::PatchSettings {
            patch: SettingsPatch::SourceControlHost {
                host: "git.example.com".into(),
                kind: tcode_core::pull_request::HostKind::Github,
                enabled: Some(true),
                account: None,
            },
        },
    );
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let snapshot = state.settings_snapshot();
        assert!(!snapshot.source_control.hosts["github.com"].enabled);
        assert_eq!(
            snapshot.source_control.hosts["github.com"]
                .account
                .as_deref(),
            Some("sample")
        );
        assert!(snapshot.source_control.hosts["git.example.com"].enabled);
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("github-writer-secret")
        );
    });
    state.dispatch_command(
        cx,
        641,
        Command::SetHostToken {
            host: "github.com".into(),
            token: None,
            email: None,
        },
    );
    cx.run_until(|_| persisted().get("@github").is_none());
    assert_eq!(
        settings_store.profile_secrets("github")["ANTHROPIC_API_KEY"],
        "provider-replacement-secret"
    );
    state.dispatch_command(
        cx,
        643,
        Command::SetProfileSecret {
            profile_id: "github".into(),
            name: "ANTHROPIC_API_KEY".into(),
            value: None,
        },
    );
    cx.run_until(|_| settings_store.profile_secrets("github").is_empty());
    let outgoing = cx.drain_outgoing();
    assert!(
        !serde_json::to_string(&outgoing)
            .unwrap()
            .contains("github-writer-secret")
    );
    assert!(
        !std::fs::read_to_string(store.root().join("settings.json"))
            .unwrap()
            .contains("github-writer-secret")
    );
    let unreadable = r#"{"version":1,"profiles":{"claude":{"KEY":"unread-secret"}}}"#;
    std::fs::write(store.root().join("secrets.json"), unreadable).unwrap();
    state.dispatch_command(
        cx,
        644,
        Command::SetHostToken {
            host: "github.com".into(),
            token: Some("replacement".into()),
            email: None,
        },
    );
    cx.run_until_sent(|message| {
        matches!(message, HostMessage::Ack { id: 644, result: Err(error) } if error.code == "settings_write_failed")
            .then_some(())
    });
    assert_eq!(
        std::fs::read_to_string(store.root().join("secrets.json")).unwrap(),
        unreadable
    );
}

#[test]
fn an_old_profiles_activity_backfills_and_updates_the_index() {
    let store = TestStore::new("activity-backfill");
    std::fs::create_dir_all(store.root()).unwrap();
    let metas: Vec<_> = (0..48)
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
            ServerEvent::IndexSummaryReplaced(summary) => activity = summary.activity,
            ServerEvent::IndexActivitiesChanged { upserts, removed } => {
                activity.extend(upserts);
                for id in removed {
                    activity.remove(&id);
                }
            }
            _ => continue,
        }
        if activity.len() == metas.len() && activity.values().all(|activity| activity.failed) {
            assert_eq!(activity.len(), metas.len());
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
            assert_eq!(snapshot.summary.activity.len(), metas.len());
            assert!(
                snapshot
                    .summary
                    .activity
                    .values()
                    .all(|activity| activity.failed)
            );
            break;
        }
    }
    link.shutdown_blocking().unwrap();
    restarted.to_host.close();
    restarted.stopped.recv_blocking().unwrap();
}

#[test]
fn inactivity_settles_on_its_due_timer_and_rearms_when_settings_change() {
    let store = TestStore::new("activity-due-timer");
    let meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
    store.upsert_meta(&meta).unwrap();
    let completed = now_millis() - 86_400_000 + 10_000;
    store
        .append_event(
            &meta.id,
            completed,
            &AgentEvent::TurnCompleted {
                turn_id: "finished".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        )
        .unwrap();
    std::fs::write(
        store.root().join("settings.json"),
        r#"{"auto_settle_after_days":null}"#,
    )
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
    link.command_blocking(Command::PatchSettings {
        patch: SettingsPatch::AutoSettleAfterDays(Some(1.0)),
    })
    .unwrap();
    loop {
        if let ServerEvent::IndexSnapshot(snapshot) = next_host_event(&events).event {
            assert!(
                !snapshot.sessions[0].is_settled(),
                "the thread is open before its delay expires"
            );
            break;
        }
    }
    assert!(
        now_millis() < completed + 86_400_000,
        "settings were enabled while the thread was still inside its inactivity window"
    );
    smol::block_on(smol::future::race(
        async {
            loop {
                if let ServerEvent::IndexUpsertSession(settled) = events.recv().await.unwrap().event
                    && settled.id == meta.id
                    && settled.is_settled()
                {
                    assert!(now_millis() > completed + 86_400_000);
                    assert_eq!(settled.settled_at, Some(completed / 1000));
                    break;
                }
            }
        },
        async {
            smol::Timer::after(Duration::from_secs(20)).await;
            panic!("due timer did not settle the thread");
        },
    ));
    link.shutdown_blocking().unwrap();
    host.to_host.close();
    host.stopped.recv_blocking().unwrap();
}

#[test]
fn deleting_the_last_child_releases_its_leads_automatic_settlement() {
    let store = TestStore::new("deleted-child-settlement");
    let stamp = now_millis() - 10 * 86_400_000;
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

#[test]
fn settle_all_children_delivers_finished_results_and_skips_running_children() {
    use tcode_core::settlement::AgentDelivery;
    use tcode_protocol::{ChildSettlement, CommandResponse, SettleChildrenResult};
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("settle-all-children");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        for (id, parent, running, native) in [
            ("lead", None, false, false),
            ("first", Some("lead"), false, false),
            ("second", Some("lead"), false, false),
            ("running", Some("lead"), true, false),
            ("other", Some("another-lead"), false, false),
            ("native", Some("lead"), false, true),
        ] {
            let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
            meta.id = id.into();
            meta.parent_session_id = parent.map(str::to_owned);
            meta.native_subagent = native.then(|| "native-item".into());
            let mut session = ActiveSession::new(meta.clone(), false, vec![]);
            session.turn_in_flight = running;
            state.sessions.push(meta);
            state.residents.parked.insert(id.into(), session);
        }
    });
    let messages = through_pipe(
        &state,
        cx,
        1,
        tcode_protocol::ClientPayload::Command(Command::SettleChildren {
            session_id: "lead".into(),
        }),
    );
    let entry = |id: &str, delivery| ChildSettlement {
        thread_id: id.into(),
        delivery,
    };
    let expected = SettleChildrenResult {
        settled: vec![
            entry("first", AgentDelivery::Settled),
            entry("second", AgentDelivery::Settled),
        ],
        skipped: vec![entry("running", AgentDelivery::Running)],
    };
    let mut result = messages
        .iter()
        .find_map(|message| match message {
            HostMessage::Ack {
                id: 1,
                result: Ok(CommandResponse::ChildrenSettled(result)),
            } => Some(result.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("settle-all response: {messages:?}"));
    result.settled.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
    assert_eq!(result, expected);
    state.read(|state| {
        for id in ["first", "second"] {
            let meta = state.find_meta(id).unwrap();
            assert!(meta.is_settled());
            assert_eq!(
                state.agent_status(&meta).unwrap().delivery,
                AgentDelivery::Settled
            );
        }
        for id in ["lead", "running", "other", "native"] {
            assert!(!state.find_meta(id).unwrap().is_settled(), "{id}");
        }
        assert!(state.resident("running").unwrap().turn_in_flight);
    });
}

/// A host over a copy of a real data dir (`TCODE_STORAGE_DATA`), run until
/// its startup storage pass reports how it ended, logging the pass to
/// stderr. Killing it anywhere and starting it again must converge on the
/// same store ([`tcode_services::store`]'s `a_real_data_dir_reads_back_as_its_original`
/// checks the result).
#[test]
#[ignore = "drives the storage pass over a copy of a real data dir named by TCODE_STORAGE_DATA; run deliberately"]
fn storage_pass_over_a_real_data_dir() {
    let mut printed = captured_log().len();
    let root = PathBuf::from(std::env::var_os("TCODE_STORAGE_DATA").unwrap());
    let started = Instant::now();
    let host = crate::pipe::spawn_host(
        SessionStore::open_at(root).unwrap(),
        crate::pipe::HostServices::default(),
    )
    .unwrap();
    eprintln!("host up in {:.1?}", started.elapsed());
    loop {
        let log = captured_log();
        let mut ended = false;
        for line in log[printed..]
            .iter()
            .filter(|line| line.target.starts_with("tcode"))
        {
            eprintln!("{} {}", line.level, line.text);
            ended |= line.text.starts_with("storage pass ");
        }
        printed = log.len();
        drop(log);
        if ended {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("pass ended after {:.1?}", started.elapsed());
    drop(host);
}

#[test]
fn a_message_after_a_restart_mid_turn_runs_a_turn_of_its_own() {
    a_message_after_its_process_went_mid_turn_runs_a_turn_of_its_own(true);
}

#[test]
fn a_message_after_the_process_exits_mid_turn_runs_a_turn_of_its_own() {
    a_message_after_its_process_went_mid_turn_runs_a_turn_of_its_own(false);
}

/// A message sent to a thread whose turn was running when its process went
/// away, with the host that restarted or on its own while the host ran,
/// opens a turn of its own that runs from the provider taking it until the
/// provider ends it, leaving the thread and coming back included; the turn
/// the gone process left ends as interrupted instead of taking the message
/// in, and after a restart it ends when its work stopped.
fn a_message_after_its_process_went_mid_turn_runs_a_turn_of_its_own(restarted: bool) {
    let test_store = TestStore::new("tcode-gone-mid-turn-test");
    let cwd = test_store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, cwd, None);
    meta.id = "thread".into();
    let task = AgentEvent::ItemCompleted(ThreadItem {
        id: "local-user-task".into(),
        parent_item_id: None,
        content: ItemContent::UserMessage {
            text: "Do the task".into(),
            context_len: None,
            attachments: Vec::new(),
        },
    });
    let work = [
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        AgentEvent::ItemStarted(ThreadItem {
            id: "build".into(),
            parent_item_id: None,
            content: ItemContent::CommandExecution {
                command: "make".into(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::InProgress,
            },
        }),
    ];
    // The host stopped an hour before the thread was opened again.
    let stopped = now_millis() - 3_600_000;
    if restarted {
        let mut cx = TestAppContext::default();
        let state = cx.new_entity(TestClientState::new((*test_store).clone()));
        state.update(&mut cx, |state, cx| {
            state.sessions.push(meta.clone());
            state.persist_meta(&meta, cx);
            let mut process = live_session(ProviderKind::ClaudeCode, smol::channel::unbounded().0);
            process.meta = meta.clone();
            state.residents.live.insert(meta.id.clone(), process);
            for event in std::iter::once(&task).chain(&work) {
                state.record_event_at("thread", stopped, event, Vec::new(), cx);
            }
        });
        cx.run_until_parked();
    } else {
        test_store.upsert_meta(&meta).unwrap();
    }

    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let commands = scripted.commands.clone();
    let mut cx = TestAppContext::default();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher_for_test(scripted.launcher);
        state
    });
    state.update(&mut cx, |state, cx| state.select_session("thread", cx));
    // The reader writes once the thread they reopened is on screen, and a
    // provider's process takes far longer to start than the host takes to
    // load the thread; the scripted one starts at once.
    cx.run_until(|state| {
        !restarted
            || state
                .resident("thread")
                .is_some_and(|session| !session.timeline.turns.is_empty())
    });
    // The host runs the turn the message opened once the provider's events,
    // which reach it off the host, have.
    let send = |cx: &mut TestAppContext, id, text: &str, turn: usize| {
        state.dispatch_command(
            cx,
            id,
            Command::SendTurn {
                session_id: "thread".into(),
                text: text.into(),
                attachment_paths: Vec::new(),
            },
        );
        let SessionCommand::SendTurn { delivery_id, .. } = cx.run_until_command(&commands) else {
            panic!("expected the message to reach the provider")
        };
        // What the Claude Code client emits once the message is written.
        for event in [
            AgentEvent::TurnAccepted { delivery_id },
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
        ] {
            scripted.events.try_send(event).unwrap();
        }
        cx.run_until(|state| {
            state.resident("thread").is_some_and(|session| {
                session.timeline.turns.get(turn).is_some_and(|turn| {
                    turn.running && turn.provider_turn_id.as_deref() == Some("turn-1")
                })
            })
        });
    };
    if !restarted {
        send(&mut cx, 1, "Do the task", 0);
        for event in [
            work[1].clone(),
            AgentEvent::SessionClosed {
                reason: Some("claude exited with exit status: 1".into()),
            },
        ] {
            scripted.events.try_send(event).unwrap();
        }
        cx.run_until(|state| {
            state
                .resident("thread")
                .is_some_and(|session| matches!(session.runtime, Runtime::Idle))
        });
    }
    send(&mut cx, 2, "continue", 1);

    // The turns a client folds from the log, and the one the host runs.
    let turns = |state: &TestEntity| {
        let log = Timeline::fold_stored(&test_store.read_events("thread").unwrap());
        let opening = |index| {
            log.entries
                .iter()
                .find(|entry| entry.turn == index)
                .map(|entry| entry.content.clone())
        };
        let statuses = log.turns.iter().map(|turn| turn.status).collect::<Vec<_>>();
        let running = state.read(|state| {
            state
                .session_status_snapshot("thread")
                .unwrap()
                .running_turn
                .map(|turn| turn.turn)
        });
        (statuses, opening(1), running)
    };
    let continued = Some(EntryContent::Item(ItemContent::UserMessage {
        text: "continue".into(),
        context_len: None,
        attachments: Vec::new(),
    }));
    let running = (
        vec![Some(TurnStatus::Interrupted), None],
        continued.clone(),
        Some(1),
    );
    assert_eq!(turns(&state), running);
    if restarted {
        let abandoned =
            Timeline::fold_stored(&test_store.read_events("thread").unwrap()).turns[0].end_ts;
        assert_eq!(
            abandoned,
            Some(stopped),
            "the abandoned turn ends when its work stopped, not when it was reopened"
        );
    }
    state.update(&mut cx, |state, cx| state.park_active(cx));
    state.reopen(&mut cx, "thread");
    assert_eq!(turns(&state), running, "coming back ended the turn");

    scripted
        .events
        .try_send(AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: TurnStatus::Completed,
            usage: None,
        })
        .unwrap();
    cx.run_until(|state| {
        state
            .resident("thread")
            .is_some_and(|session| !session.turn_in_flight)
    });
    assert_eq!(
        turns(&state),
        (
            vec![Some(TurnStatus::Interrupted), Some(TurnStatus::Completed)],
            continued,
            None,
        )
    );
}
