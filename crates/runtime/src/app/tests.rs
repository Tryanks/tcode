use super::test_support::*;
use super::*;
use super::{active_session::*, events::*, orchestrate::*, providers::*};

use tcode_core::settings::{SettingsPatch, ThemeMode};
use tcode_protocol::{Command, CommandResponse, HostMessage};
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
        parent_meta.approval_mode = ApprovalMode::ReadOnly;
        parent_meta.interaction_mode = InteractionMode::Plan;
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
        assert_eq!(mirror.approval_mode, ApprovalMode::ReadOnly);
        assert_eq!(mirror.interaction_mode, InteractionMode::Plan);
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
                },
            }),
            cx,
        );
        let mirror_timeline = &state.resident(&mirror.id).unwrap().timeline;
        assert!(mirror_timeline.entries.iter().any(|entry| {
            matches!(
                &entry.content,
                EntryContent::Item(ItemContent::AssistantMessage { text })
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

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_status("parent".into(), None, reply, cx);
        assert_eq!(response.try_recv().unwrap().unwrap(), serde_json::json!([]));
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
            .into_iter()
            .find(|meta| meta.id == mirror.id)
            .unwrap();
        assert_eq!(persisted.model.as_deref(), Some("child-model"));
        assert_eq!(
            persisted.option_selections[0].value,
            serde_json::json!("low")
        );
        assert_eq!(persisted.interaction_mode, InteractionMode::Plan);
        let mirror_events = state.store.read_events(&mirror.id);
        assert!(mirror_events.iter().any(|stored| matches!(
            &stored.event,
            AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                ..
            }) if id == "child-answer"
        )));
        let parent_events = state.store.read_events("parent");
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
        let events = state.store.read_events(&mirror_id);
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
    cx.run_until(|state| {
        state.resident(&mirror_id).is_some_and(|mirror| {
            mirror
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
        assert!(!state.session_status_snapshot(&mirror_id).unwrap().working);
        assert_eq!(
            mirror
                .timeline
                .entries
                .iter()
                .filter(|entry| matches!(
                    entry.content,
                    EntryContent::Item(ItemContent::AssistantMessage { .. })
                ))
                .count(),
            expected_items
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
/// content of the child mirror, and once the grandchild's own transcript
/// arrives it gets a titled mirror under the child mirror — never a nameless
/// one on the root — whose turn closes with the nested spawn's terminal status.
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
        assert!(
            state
                .sessions
                .iter()
                .all(|meta| meta.native_subagent.as_deref() != Some("toolu_grandchild")),
            "a spawn item alone is content of the child mirror, not a mirror"
        );
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
        let grandchild = state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.as_deref() == Some("toolu_grandchild"))
            .cloned()
            .expect("grandchild mirror");
        assert_eq!(
            grandchild.parent_session_id.as_deref(),
            Some(child_id.as_str())
        );
        assert_eq!(grandchild.title, "Explore: Research Workers limits");
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
        let events = state.store.read_events(&grandchild_id);
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
            &state.store.read_events(&second_id).last().unwrap().event,
            AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Interrupted, .. }
                if turn_id == "toolu_grandchild_2"
        ));
        assert!(
            state
                .store
                .read_events(&child_id)
                .iter()
                .all(|stored| match &stored.event {
                    AgentEvent::ItemStarted(item)
                    | AgentEvent::ItemUpdated(item)
                    | AgentEvent::ItemCompleted(item) => item.parent_item_id.is_none(),
                    _ => true,
                })
        );
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
        let events = state.store.read_events(&mirror_id);
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
        let events = state.store.read_events("mirror");
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[2].event,
            AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Interrupted, .. }
                if turn_id == "toolu_zombie"
        ));
        assert_eq!(state.native_subagent_turns.get("mirror"), Some(&false));
    });

    // Reloading the repaired mirror leaves the closed turn alone.
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    state.update(cx, |state, cx| state.select_session("mirror", cx));
    cx.run_until(|state| {
        state
            .resident("mirror")
            .is_some_and(|mirror| mirror.timeline.turns.len() == 1)
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        assert_eq!(state.store.read_events("mirror").len(), 3);
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
        auto_archive_max_idle_days: 99,
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
    settings.collapsed_threads.push("parent".into());
    settings.favorite_models.push("gpt-5.6-sol".into());
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
        assert_eq!(reset.auto_archive_max_idle_days, 7);
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
        assert_eq!(reset.collapsed_threads, vec!["parent".to_string()]);
        assert_eq!(reset.favorite_models, vec!["gpt-5.6-sol".to_string()]);
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

#[test]
fn parked_session_projection_diff_emits_session_status() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-parked-session-diff-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let parked = live_session(ProviderKind::Codex, smol::channel::unbounded().0);
    let parked_id = parked.meta.id.clone();

    state.update(cx, |state, _cx| {
        state.sessions.push(parked.meta.clone());
        state
            .residents
            .parked
            .insert(parked.meta.id.clone(), parked);
    });
    cx.run_until_parked();
    cx.drain_outgoing();

    state.dispatch_command(
        cx,
        45,
        Command::RenameSession {
            session_id: parked_id.clone(),
            title: "Renamed while parked".into(),
        },
    );
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    let statuses: Vec<_> = outgoing
        .iter()
        .filter_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus { session_id },
                event: ServerEvent::SessionStatusReplaced(status),
            }) if session_id == &parked_id => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].title, "Renamed while parked");
    state.read(|state| {
        assert!(state.selected_session().is_none());
        assert!(state.residents.parked.contains_key(&parked_id));
    });
}

#[test]
fn dispatched_start_draft_emits_session_status_over_ndjson() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-dispatch-session-status-seam-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let cwd = test_store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    state.dispatch_command(
        cx,
        42,
        Command::StartDraft {
            project_id: "project-1".into(),
            cwd: cwd.clone(),
        },
    );
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope { request_id: None,
            topic: Topic::SessionStatus { session_id },
            event: ServerEvent::SessionStatusReplaced(status),
            ..
        }) if session_id == &status.session_id
            && status.project_id.as_deref() == Some("project-1")
            && status.cwd == cwd
            && status.draft
    )));
}

#[test]
fn scripted_provider_connects_command_launch_and_agent_event_paths() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scripted-provider-seam-test");
    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let commands = scripted.commands.clone();
    let events = scripted.events.clone();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher_for_test(scripted.launcher);
        state
    });
    let cwd = test_store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    state.dispatch_command(
        cx,
        51,
        Command::StartDraft {
            project_id: "project-1".into(),
            cwd,
        },
    );
    let session_id = state.read(|state| state.residents.live.keys().next().unwrap().clone());
    state.update(cx, |state, cx| state.select_session(&session_id, cx));
    state.dispatch_command(
        cx,
        52,
        Command::SendTurn {
            session_id,
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

    let outgoing = cx.drain_outgoing();
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope { request_id: None,
            topic: Topic::SessionEvents { .. },
            event: ServerEvent::SessionEvent(SessionEventRecord {
                event: AgentEvent::TurnStarted { turn_id },
                ..
            }),
            ..
        }) if turn_id == "scripted-turn"
    )));
    state.read(|state| {
        assert!(state.selected_session().unwrap().turn_in_flight);
    });
}

#[test]
fn host_start_folds_only_parents_with_visible_children() {
    let mut archived_child = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/p"), None);
    archived_child.id = "archived-child".into();
    archived_child.parent_session_id = Some("quiet-parent".into());
    archived_child.archived_at = Some(1);
    let mut sessions = Vec::new();
    for (id, parent) in [
        ("parent-a", None),
        ("child-a", Some("parent-a")),
        ("parent-b", None),
        ("child-b", Some("parent-b")),
        ("grandchild-b", Some("child-b")),
        ("plain", None),
        ("quiet-parent", None),
        ("orphan", Some("missing-parent")),
    ] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/p"), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_string);
        sessions.push(meta);
    }
    sessions.push(archived_child);

    assert_eq!(
        startup_collapsed_threads(&sessions),
        ["parent-a", "parent-b", "child-b"].map(String::from),
        "every parent of a visible child starts folded; leaves, a parent \
         whose only child is archived, and a parent id no session carries \
         (its orphans would be hidden with no row to unfold) do not"
    );
}

#[test]
fn thread_fold_is_host_state_shared_over_the_pipe_and_pruned_with_the_thread() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-thread-fold-test");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    for (id, parent) in [("parent", None), ("child", Some("parent"))] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_string);
        store.upsert_meta(&meta).unwrap();
    }
    let state = cx.new_entity(TestClientState::new(store.clone()));
    state.update(cx, |state, _| {
        assert_eq!(
            state.settings.collapsed_threads,
            vec!["parent".to_string()],
            "a parent that already has a child starts folded"
        );
    });
    cx.run_until_parked();
    cx.drain_outgoing();

    state.dispatch_command(
        cx,
        7,
        Command::SetThreadCollapsed {
            session_id: "parent".into(),
            collapsed: false,
        },
    );
    state.dispatch_command(
        cx,
        8,
        Command::SetThreadCollapsed {
            session_id: "missing".into(),
            collapsed: true,
        },
    );
    cx.run_until_parked();
    let outgoing = cx.drain_outgoing();
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Ack {
            id: 7,
            result: Ok(CommandResponse::Unit)
        }
    )));
    assert!(
        outgoing.iter().any(|message| matches!(message,
            HostMessage::Ack { id: 8, result: Err(error) } if error.code == "unknown_session"
        )),
        "folding an unknown thread is rejected instead of stored"
    );
    assert!(
        outgoing.iter().any(|message| matches!(message,
            HostMessage::Event(EventEnvelope {
                topic: Topic::Settings,
                event: ServerEvent::SettingsReplaced(settings),
                ..
            }) if settings.collapsed_threads.is_empty()
        )),
        "every client learns the fold from the settings topic"
    );

    state.update(cx, |state, cx| {
        state.set_thread_collapsed("parent", true, cx);
        assert_eq!(state.settings.collapsed_threads, vec!["parent".to_string()]);
        state.delete_session("parent", false, cx);
        assert!(
            state.settings.collapsed_threads.is_empty(),
            "a deleted thread leaves no fold behind"
        );
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
    let persisted = store.load_index();
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
fn generated_titles_are_cleaned_and_bounded() {
    assert_eq!(
        sanitize_generated_title("  **Title: Fix sidebar rename.**  ").as_deref(),
        Some("Fix sidebar rename")
    );
    assert_eq!(
        sanitize_generated_title("# 「标题：为对话生成简洁标题。」").as_deref(),
        Some("为对话生成简洁标题")
    );
    assert_eq!(sanitize_generated_title("  ` `  "), None);

    let long = "a".repeat(TITLE_MAX_CHARS + 10);
    let title = sanitize_generated_title(&long).unwrap();
    assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
    assert!(title.ends_with('…'));
}

#[test]
fn provider_diagnostics_with_zero_output_tokens_are_not_titles() {
    let mut usage = agent::TokenUsage {
        output_tokens: Some(0),
        ..Default::default()
    };
    assert!(!title_turn_generated_output(
        TurnStatus::Completed,
        Some(&usage)
    ));

    usage.output_tokens = Some(4);
    assert!(title_turn_generated_output(
        TurnStatus::Completed,
        Some(&usage)
    ));
    assert!(title_turn_generated_output(TurnStatus::Completed, None));
    assert!(!title_turn_generated_output(TurnStatus::Failed, None));
}

#[test]
fn title_prompt_treats_the_request_as_bounded_json_data() {
    let escaped = title_generation_prompt("line one\nline two", None, true);
    assert!(escaped.contains("untrusted source text"));
    assert!(escaped.contains("original image attachments"));
    assert!(escaped.contains("\\n"), "the request is JSON escaped");

    let source = "界".repeat(TITLE_SOURCE_MAX_CHARS + 20);
    let prompt = title_generation_prompt(&source, None, false);
    assert!(prompt.contains("untrusted source text"));
    assert!(!prompt.contains(&"界".repeat(TITLE_SOURCE_MAX_CHARS + 1)));
    assert!(prompt.contains(&format!("{}…", "界".repeat(TITLE_SOURCE_MAX_CHARS))));
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
    assert_eq!(custom.approval_mode, ApprovalMode::Supervised);
    assert_eq!(custom.interaction_mode, InteractionMode::Build);
    assert!(!custom.orchestrate_enabled);
    assert_eq!(
        title_turn_options().effort.as_deref(),
        Some(AI_TITLE_REASONING_EFFORT)
    );

    settings.title_generation.profile_id = Some("deleted-profile".into());
    let fallback = title_session_meta(&settings, PathBuf::from("/tmp/project"));
    assert_eq!(fallback.profile_id, None);
}

#[test]
fn fallback_review_separates_assessment_and_draft_without_inventing_missing_text() {
    for (input, assessment, draft) in [
        (
            "ASSESSMENT: Specific scope.\n---DRAFT---\nI own the test system.",
            "Specific scope.",
            "I own the test system.",
        ),
        (
            "Assessment without a separator.",
            "Assessment without a separator.",
            "",
        ),
        ("ASSESSMENT: Concerning.\n---DRAFT---\n", "Concerning.", ""),
        (
            "  assessment   :   Likely benign.\n  ---DRAFT---  \n  I administer this host.  ",
            "Likely benign.",
            "I administer this host.",
        ),
        ("", "", ""),
        ("---DRAFT---\nDraft only.", "", "Draft only."),
        ("评估内容\n---DRAFT---\n申诉内容", "评估内容", "申诉内容"),
    ] {
        assert_eq!(
            parse_fallback_review(input),
            (assessment.into(), draft.into()),
            "input: {input:?}",
        );
    }
}

#[test]
fn title_regeneration_uses_stored_history_and_preserves_intervening_changes() {
    for outcome in ["generated", "cached", "renamed", "deleted", "failed"] {
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
            HostMessage::Event(EventEnvelope { event: ServerEvent::IndexSnapshot(snapshot), .. })
                if snapshot.title_generating.contains(&id)
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
                        text: "Improve QR Sharing".into(),
                    },
                }))
                .unwrap();
            scripted
                .events
                .try_send(AgentEvent::TurnCompleted {
                    turn_id: "title-turn".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                })
                .unwrap();
        }
        scripted
            .events
            .try_send(AgentEvent::SessionClosed { reason: None })
            .unwrap();
        // Completion arrives after the scratch-cwd cleanup on the thread pool;
        // wait on the pending flag rather than a fixed idle window.
        cx.run_until(|state| !state.index_snapshot().title_generating.contains(&id));
        state.read(|state| {
            let title = state
                .sessions
                .iter()
                .find(|meta| meta.id == id)
                .map(|meta| meta.title.as_str());
            let expected = match outcome {
                "generated" | "cached" => Some("Improve QR Sharing"),
                "renamed" => Some("My title"),
                "deleted" => None,
                "failed" => Some("Old title"),
                _ => unreachable!(),
            };
            assert_eq!(title, expected, "{outcome}");
            let persisted = store.load_index().into_iter().find(|meta| meta.id == id);
            assert_eq!(persisted.as_ref().map(|meta| meta.title.as_str()), expected);
            if matches!(outcome, "generated" | "cached" | "failed") {
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
    cx.run_until(|state| !state.index_snapshot().title_generating.contains(&id));
    assert!(scripted.commands.try_recv().is_err());
    state.read(|state| {
        assert!(!state.index_snapshot().title_generating.contains(&id));
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
fn marketplace_items_are_runtime_owned_views() {
    let test_store = TestStore::new("tcode-marketplace-view-test");
    let store = (*test_store).clone();
    let mut state = TestClientState::new(store);
    state.acp_registry = Some(
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

#[test]
fn orchestrate_guidance_and_current_configuration_are_composed() {
    let mut settings = OrchestrateSettings::default();
    let first = compose_orchestrate_text(&settings, "Ship it", None, &HashMap::new());
    assert!(first.starts_with(ORCHESTRATE_GUIDANCE.trim()));
    assert!(first.contains("### Collaboration models — `collaborate`"));
    assert!(first.contains("#### `codex` / `gpt-6-astra` — available `effort`: `medium`, `high`"));
    assert!(
        first.contains("#### `claude` / `claude-fable-5-1` — available `effort`: `medium`, `high`")
    );
    assert!(first.contains("### Execution models — `dispatch`"));
    assert!(first.contains(
        "#### `codex` / `gpt-6-astra` — available `effort`: `low`, `medium`, `high`, `xhigh`, `max`"
    ));
    assert!(first.ends_with("\n\nShip it"));
    settings.decision_models[0].enabled = false;
    settings.child_models.clear();
    let follow_up = compose_orchestrate_text(&settings, "Follow up", None, &HashMap::new());
    assert!(follow_up.starts_with(ORCHESTRATE_GUIDANCE.trim()));
    assert!(follow_up.contains("## Current orchestrator configuration"));
    assert!(!follow_up.contains("#### `codex` / `gpt-6-astra`"));
    assert!(follow_up.contains("`dispatch` is unavailable"));
    assert!(follow_up.ends_with("\n\nFollow up"));
}

#[test]
fn lead_sees_only_other_peers_and_each_peer_receives_its_own_guidance() {
    let settings = OrchestrateSettings::default();
    for (index, name, other_name) in [(0, "Astra", "Fable 5.1"), (1, "Fable 5.1", "Astra")] {
        let peer = &settings.decision_models[index];
        let composed = compose_orchestrate_text(
            &settings,
            "Discuss the design",
            Some((peer.provider, Some(peer.model.as_str()))),
            &HashMap::new(),
        );
        assert!(!composed.contains(&format!("You are {name},")));
        assert!(composed.contains(&format!("You are {other_name},")));
        let brief = compose_collaboration_brief(
            &settings,
            peer.provider,
            &peer.model,
            "An independent view",
        );
        assert!(brief.contains(&format!("You are {name},")));
        assert!(!brief.contains(&format!("You are {other_name},")));
        assert!(brief.ends_with("An independent view"));
    }
    let unknown = compose_orchestrate_text(
        &settings,
        "Discuss",
        Some((ProviderKind::Codex, None)),
        &HashMap::new(),
    );
    assert!(!unknown.contains("You are Astra,"));
    assert!(unknown.contains("You are Fable 5.1,"));
}

#[test]
fn dispatch_validates_against_live_efforts_instead_of_bundled_fallback() {
    let settings = OrchestrateSettings::default();
    let catalogs = HashMap::from([(
        ProviderKind::Codex,
        vec![ModelSpec {
            id: "gpt-6-astra".into(),
            display_name: "GPT-6 Astra".into(),
            is_default: false,
            options: vec![OptionDescriptor::Select {
                id: "reasoningEffort".into(),
                label: "Effort".into(),
                default_value: Some("high".into()),
                options: ["medium", "high", "deep"]
                    .into_iter()
                    .map(|value| agent::SelectOption {
                        value: value.into(),
                        label: value.into(),
                        description: None,
                    })
                    .collect(),
            }],
        }],
    )]);
    assert_eq!(
        resolve_orchestrate_dispatch(&settings, "codex", None, Some("deep"), None, &catalogs)
            .unwrap()
            .2
            .as_deref(),
        Some("deep")
    );
    assert!(
        resolve_orchestrate_dispatch(&settings, "codex", None, Some("max"), None, &catalogs)
            .unwrap_err()
            .contains("unsupported effort max")
    );
    let configuration = render_orchestrate_configuration(&settings, None, &catalogs);
    assert!(configuration.contains("`gpt-6-astra` — available `effort`: `medium`, `high`, `deep`"));
}

#[test]
fn loaded_catalog_marks_missing_orchestrate_model_unavailable() {
    let settings = OrchestrateSettings::default();
    let catalogs = HashMap::from([(
        ProviderKind::Codex,
        vec![ModelSpec {
            id: "gpt-5.6-terra".into(),
            display_name: "Terra".into(),
            is_default: false,
            options: Vec::new(),
        }],
    )]);
    let expected = "model `gpt-6-astra` is unavailable for provider `codex`: not present in the loaded catalog";

    assert_eq!(
        resolve_orchestrate_dispatch(
            &settings,
            "codex",
            Some("gpt-6-astra"),
            Some("low"),
            None,
            &catalogs
        )
        .unwrap_err(),
        expected
    );
    assert_eq!(
        resolve_orchestrate_collaboration(
            &settings,
            "codex",
            Some("gpt-6-astra"),
            Some("medium"),
            None,
            &catalogs
        )
        .unwrap_err(),
        expected
    );
    let configuration = render_orchestrate_configuration(&settings, None, &catalogs);
    assert!(configuration.contains("#### `codex` / `gpt-6-astra` — unavailable"));
    assert!(configuration.contains(
        "Unavailable: model `gpt-6-astra` is not present in the loaded `codex` catalog."
    ));
}

#[test]
fn collaboration_and_execution_resolve_separate_profile_lists() {
    let mut settings = OrchestrateSettings::default();
    assert_eq!(
        resolve_orchestrate_collaboration(&settings, "codex", None, None, None, &HashMap::new())
            .unwrap()
            .1,
        "gpt-6-astra"
    );
    assert_eq!(
        resolve_orchestrate_collaboration(&settings, "claude", None, None, None, &HashMap::new())
            .unwrap()
            .1,
        "claude-fable-5-1"
    );
    assert!(
        resolve_orchestrate_collaboration(
            &settings,
            "codex",
            Some("gpt-6-astra"),
            Some("high"),
            None,
            &HashMap::new()
        )
        .is_ok()
    );
    assert_eq!(
        resolve_orchestrate_dispatch(
            &settings,
            "codex",
            Some("gpt-6-astra"),
            Some("low"),
            None,
            &HashMap::new()
        )
        .unwrap()
        .2
        .as_deref(),
        Some("low")
    );
    assert!(
        resolve_orchestrate_dispatch(
            &settings,
            "claude",
            Some("claude-fable-5-1"),
            None,
            None,
            &HashMap::new()
        )
        .is_err()
    );
    settings.decision_models[0].enabled = false;
    assert!(
        resolve_orchestrate_collaboration(&settings, "codex", None, None, None, &HashMap::new())
            .is_err()
    );
    // Disabling one role leaves the other role's row untouched.
    assert_eq!(
        resolve_orchestrate_dispatch(
            &settings,
            "codex",
            Some("gpt-6-astra"),
            Some("low"),
            None,
            &HashMap::new()
        )
        .unwrap()
        .1,
        "gpt-6-astra"
    );
    settings.decision_models[0].enabled = true;
    settings.child_models[0].enabled = false;
    assert!(
        resolve_orchestrate_dispatch(&settings, "codex", None, None, None, &HashMap::new())
            .is_err()
    );
    assert!(
        resolve_orchestrate_collaboration(&settings, "codex", None, None, None, &HashMap::new())
            .is_ok()
    );
    settings.child_models[0].enabled = true;
    settings.decision_models[0].profile_id = Some("custom".into());
    assert!(
        resolve_orchestrate_collaboration(
            &settings,
            "codex",
            None,
            Some("max"),
            Some("custom"),
            &HashMap::new()
        )
        .is_err()
    );
    assert_eq!(
        resolve_orchestrate_collaboration(
            &settings,
            "codex",
            None,
            Some("high"),
            Some("custom"),
            &HashMap::new()
        )
        .unwrap()
        .4
        .as_deref(),
        Some("custom")
    );
}

#[test]
fn collaboration_starts_a_read_only_peer_discussion() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-peer-collaboration-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    state.update(cx, |state, cx| {
        let parent = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/workspace"),
            Some("claude-fable-5-1".into()),
        );
        let parent_id = parent.id.clone();
        state.sessions.push(parent);
        state.settings.orchestrate.child_worktrees = true;
        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Dispatch {
                purpose: orchestrate_mcp::ThreadPurpose::Collaboration,
                parent_id: parent_id.clone(),
                provider: "codex".into(),
                model: None,
                effort: None,
                profile: None,
                access: Some("full".into()),
                title: "Compare architectures".into(),
                brief: "Challenge these alternatives".into(),
                cwd: None,
                worktree: Some(true),
                archive_on_complete: None,
                result_max_chars: Some(0),
                fast: None,
            },
            reply,
            cx,
        );
        let result = response.try_recv().unwrap().unwrap();
        let id = result["thread_id"].as_str().unwrap();
        let child = state.resident(id).unwrap();
        assert_eq!(child.meta.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(
            child.meta.parent_session_id.as_deref(),
            Some(parent_id.as_str())
        );
        assert_eq!(child.meta.approval_mode, ApprovalMode::ReadOnly);
        assert!(child.meta.worktree.is_none());
        assert!(!child.meta.orchestrate_enabled);
        assert!(
            child.queue[0]
                .text
                .starts_with(COLLABORATION_GUIDANCE.trim())
        );
        assert!(child.queue[0].text.contains("Challenge these alternatives"));
        assert!(
            child.queue[0]
                .text
                .contains("You are Astra, a peer collaborator")
        );
        assert!(!child.queue[0].text.contains("You are Fable"));
        assert!(child.queue[0].text.ends_with(CHILD_REPORT_FOOTER));
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

        state.send_turn("assembled", "Explain this".into(), vec![attachment_path.clone()], cx);

        let SessionCommand::SendTurn {
            text, attachments, ..
        } = receiver.try_recv().expect("assembled send command")
        else {
            panic!("expected SendTurn")
        };
        assert_eq!(
            text,
            "Explain this\n\n<terminal_context>\n- zsh lines 12-13:\n  12 | cargo test\n  13 | ok\n</terminal_context>\n\n<review_comment sectionId=\"section\" sectionTitle=\"Changes\" filePath=\"src/lib.rs\" startIndex=\"3\" endIndex=\"4\" rangeLabel=\"+7\">\nPlease fix\n```diff\nlet bad = true;\n```\n</review_comment>"
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

#[test]
fn orchestrate_turn_records_context_and_runs_with_collaboration_disabled() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-split-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, receiver) = smol::channel::unbounded();
    let sent_text = state.update(cx, |state, cx| {
        // Keep the provider live so the send exercises ordinary delivery and
        // records its context split after acceptance.
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "orchestrator".into();
        active.meta.model = Some("gpt-6-astra".into());
        active.meta.orchestrate_enabled = true;
        // Disabling every peer (including Astra itself) must only block
        // consultation. Astra remains the lead and can dispatch execution.
        for peer in &mut state.settings.orchestrate.decision_models {
            peer.enabled = false;
        }
        assert!(
            resolve_orchestrate_collaboration(
                &state.settings.orchestrate,
                "codex",
                None,
                None,
                None,
                &state.providers.model_catalogs
            )
            .is_err()
        );
        assert!(
            resolve_orchestrate_dispatch(
                &state.settings.orchestrate,
                "codex",
                None,
                None,
                None,
                &state.providers.model_catalogs
            )
            .is_ok()
        );
        // Match the live launch state so the send is an ordinary turn rather
        // than a restart (which would flush through a different path).
        active.live_model = active.meta.model.clone();
        active.live_approval_mode = Some(active.meta.approval_mode);
        state.install_selected(active);

        state.orchestrate_turn("orchestrator", "执行某某任务".into(), Vec::new(), cx);
        let (delivery_id, text) = match receiver.try_recv() {
            Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) => (delivery_id, text),
            other => panic!("expected orchestrator SendTurn, got {other:?}"),
        };
        state.on_event("orchestrator", AgentEvent::TurnAccepted { delivery_id }, cx);

        assert!(text.ends_with("执行某某任务"));
        assert!(text.len() > "执行某某任务".len());
        text
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let events = state.store.read_events("orchestrator");
        let recorded = events
            .iter()
            .find_map(|stored| match &stored.event {
                AgentEvent::ItemCompleted(ThreadItem {
                    content:
                        ItemContent::UserMessage {
                            text, context_len, ..
                        },
                    ..
                }) => Some((text.clone(), *context_len)),
                _ => None,
            })
            .expect("orchestrate turn recorded a user message");
        assert_eq!(recorded.0, sent_text);
        assert_eq!(recorded.1, Some(sent_text.len() - "执行某某任务".len()));

        // Folded, the timeline splits the prefix from the user's own words.
        let timeline = Timeline::fold_events(events);
        let user = timeline
            .entries
            .iter()
            .find_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::UserMessage {
                    text,
                    context_len: Some(len),
                    ..
                }) => Some((text.clone(), *len)),
                _ => None,
            })
            .expect("folded user entry carries the split");
        assert_eq!(&user.0[user.1..], "执行某某任务");
    });
}

#[test]
fn orchestrate_title_generation_uses_only_the_users_request() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-title-source-test");
    let scripted_title = scripted_provider(ProviderKind::Codex);
    let title_commands = scripted_title.commands.clone();
    let title_events = scripted_title.events.clone();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.ai_title_generation_enabled = true;
        state.set_provider_launcher_for_test(scripted_title.launcher);
        state
    });
    let (commands, receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "orchestrator-title".into();
        active.meta.orchestrate_enabled = true;
        active.live_model = active.meta.model.clone();
        active.live_approval_mode = Some(active.meta.approval_mode);
        state.install_selected(active);

        state.orchestrate_turn("orchestrator-title", "执行某某任务".into(), Vec::new(), cx);
        let delivery_id = match receiver.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected orchestrator SendTurn, got {other:?}"),
        };
        state.on_event(
            "orchestrator-title",
            AgentEvent::TurnAccepted { delivery_id },
            cx,
        );
    });
    cx.run_until(|_| !title_commands.is_empty());

    let title_prompt = match title_commands.try_recv() {
        Ok(SessionCommand::SendTurn { text, .. }) => text,
        other => panic!("expected title-generation SendTurn, got {other:?}"),
    };
    assert_eq!(
        title_prompt,
        title_generation_prompt("执行某某任务", None, false),
        "the hidden orchestrate prefix must not be sent to the title model"
    );

    title_events
        .try_send(AgentEvent::ItemCompleted(ThreadItem {
            id: "title".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "执行某某任务".into(),
            },
        }))
        .unwrap();
    title_events
        .try_send(AgentEvent::TurnCompleted {
            turn_id: "title-turn".into(),
            status: TurnStatus::Completed,
            usage: None,
        })
        .unwrap();
    title_events
        .try_send(AgentEvent::SessionClosed { reason: None })
        .unwrap();
    cx.run_until(|state| {
        !state
            .index_snapshot()
            .title_generating
            .contains("orchestrator-title")
    });
}

#[test]
fn orchestrate_dispatch_enforces_child_allow_list_and_defaults() {
    let mut settings = OrchestrateSettings::default();
    assert_eq!(
        resolve_orchestrate_dispatch(
            &settings,
            "codex",
            Some("gpt-6-astra"),
            Some("low"),
            None,
            &HashMap::new()
        )
        .unwrap(),
        (
            ProviderKind::Codex,
            "gpt-6-astra".into(),
            Some("low".into()),
            false,
            None
        )
    );
    settings.child_models[0].profile_id = Some("kimi".into());
    assert_eq!(
        resolve_orchestrate_dispatch(
            &settings,
            "codex",
            Some("gpt-6-astra"),
            Some("medium"),
            Some("KIMI"),
            &HashMap::new()
        )
        .unwrap(),
        (
            ProviderKind::Codex,
            "gpt-6-astra".into(),
            Some("medium".into()),
            false,
            Some("kimi".into()),
        )
    );
    let unknown_profile = resolve_orchestrate_dispatch(
        &settings,
        "codex",
        Some("gpt-6-astra"),
        Some("medium"),
        Some("missing"),
        &HashMap::new(),
    )
    .unwrap_err();
    assert!(unknown_profile.contains("profile missing"));
    assert!(unknown_profile.contains("profile kimi"));
    assert_eq!(
        resolve_orchestrate_dispatch(
            &settings,
            "claude_code",
            Some("claude-opus-5"),
            Some(" HIGH "),
            None,
            &HashMap::new()
        )
        .unwrap(),
        (
            ProviderKind::ClaudeCode,
            "claude-opus-5".into(),
            Some("high".into()),
            false,
            None
        )
    );
    for effort in ["low", "medium", "high", "xhigh", "max"] {
        assert_eq!(
            resolve_orchestrate_dispatch(
                &settings,
                "codex",
                Some("gpt-6-astra"),
                Some(effort),
                None,
                &HashMap::new()
            )
            .unwrap()
            .2
            .as_deref(),
            Some(effort)
        );
    }
    let wrong_effort = resolve_orchestrate_dispatch(
        &settings,
        "codex",
        Some("gpt-6-astra"),
        Some("imaginary"),
        None,
        &HashMap::new(),
    )
    .unwrap_err();
    assert!(wrong_effort.contains("unsupported effort imaginary"));
    assert!(wrong_effort.contains("low, medium, high, xhigh, max"));
    let denied = resolve_orchestrate_dispatch(
        &settings,
        "claude",
        Some("claude-haiku-4-5"),
        None,
        None,
        &HashMap::new(),
    )
    .unwrap_err();
    assert!(denied.contains("no enabled profile matches"));

    let mut empty = settings;
    empty.child_models.clear();
    assert!(
        resolve_orchestrate_dispatch(&empty, "codex", None, None, None, &HashMap::new())
            .unwrap_err()
            .contains("enabled profiles: none")
    );
}

#[test]
fn orchestrate_dispatch_access_maps_known_values() {
    assert_eq!(resolve_dispatch_access(None), Ok(ApprovalMode::FullAccess));
    assert_eq!(
        resolve_dispatch_access(Some(" FULL ")),
        Ok(ApprovalMode::FullAccess)
    );
    assert_eq!(
        resolve_dispatch_access(Some("read_only")),
        Ok(ApprovalMode::ReadOnly)
    );
    assert_eq!(
        resolve_dispatch_access(Some("WORKSPACE_WRITE")),
        Ok(ApprovalMode::AutoAcceptEdits)
    );
    assert_eq!(
        resolve_dispatch_access(Some("admin")),
        Err("unknown access: admin; expected read_only, workspace_write, or full".into())
    );
}

#[test]
fn updates_on_the_viewed_thread_do_not_mark_it_unread() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-viewed-unread-test");
    let store = (*test_store).clone();
    let first = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/a"), None);
    let second = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/b"), None);
    store.upsert_meta(&first).unwrap();
    store.upsert_meta(&second).unwrap();
    let first_id = first.id.clone();
    let second_id = second.id.clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        state.select_session(&first_id, cx);

        // A turn finishes while the user is watching: updated_at moves past
        // the watermark stamped on entry, and the meta is persisted.
        let active = state.selected_session_mut().unwrap();
        active.meta.updated_at = now_secs() + 10;
        let meta = active.meta.clone();
        state.persist_meta(&meta, cx);

        // Switching away must not surface an unread dot for what the user
        // already saw happen on screen.
        state.select_session(&second_id, cx);
        assert!(!state.session_unread(&first_id));

        // But an update landing on a thread the user is NOT viewing still
        // marks it unread.
        let mut parked = state
            .sessions
            .iter()
            .find(|m| m.id == first_id)
            .cloned()
            .unwrap();
        parked.updated_at = now_secs() + 20;
        state.persist_meta(&parked, cx);
        assert!(state.session_unread(&first_id));
    });
}

#[test]
fn draft_send_creates_session_with_project_cwd() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-draft-test");
    let store = (*test_store).clone();
    let project = Project::from_root(PathBuf::from("/tmp/tcode-draft-proj"));
    // Persist the project so the draft's project_id survives index migration.
    store.upsert_project(&project).unwrap();
    let state = cx.new_entity(TestClientState::new(store));
    // A draft is set up (cwd = project root) but not yet persisted.
    let draft = AppState::build_draft_session(
        project.id.clone(),
        project.root.clone(),
        ProviderKind::ClaudeCode,
        None,
        None,
        Vec::new(),
    );
    assert!(draft.draft);
    assert_eq!(draft.meta.cwd, project.root);
    assert_eq!(draft.meta.project_id.as_deref(), Some(project.id.as_str()));
    assert!(matches!(draft.runtime, Runtime::Idle));
    let draft_id = draft.meta.id.clone();
    state.update(cx, |state, cx| {
        state.install_selected(draft);
        // Not in the index until the first send materializes it.
        assert!(!state.sessions.iter().any(|m| m.id == draft_id));

        // The first send commits the draft: it becomes a real session whose
        // cwd is the project root and shows up in the sidebar index.
        state.commit_draft(&draft_id, cx);
        assert!(!state.selected_session().unwrap().draft);
        let created = state.sessions.iter().find(|m| m.id == draft_id).unwrap();
        assert_eq!(created.cwd, project.root);
        assert_eq!(created.project_id.as_deref(), Some(project.id.as_str()));
    });
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
            state.sessions = sessions;
            state.start_draft("target".into(), PathBuf::from("/new-project"), cx);
            let draft = state.selected_session().unwrap();
            assert!(draft.draft, "{name}");
            assert_eq!(draft.meta.provider, provider, "{name}");
            assert_eq!(draft.meta.model.as_deref(), model, "{name}");
            assert_eq!(draft.meta.acp_agent_id.as_deref(), agent, "{name}");
            assert_eq!(draft.meta.profile_id.as_deref(), profile, "{name}");
            assert_eq!(draft.meta.cwd, PathBuf::from("/new-project"));
            assert_eq!(draft.meta.project_id.as_deref(), Some("target"));
            let expected = if effort {
                vec![OptionSelection {
                    id: "reasoningEffort".into(),
                    value: serde_json::json!("high"),
                }]
            } else {
                vec![]
            };
            assert_eq!(draft.meta.option_selections, expected, "{name}");
            assert!(state.store.load_index().is_empty(), "{name}");
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
        state.sessions = vec![previous];
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
        assert!(draft.meta.option_selections.is_empty());
        assert!(state.store.load_index().is_empty());
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
        state.sessions = vec![sol, fable];
        state.start_draft("project-target".into(), PathBuf::from("/tmp/target"), cx);

        // Switching the draft to a model brings back the effort it last ran at,
        // not the model's default.
        state.host.set_active_model(
            state.selected.as_deref().unwrap_or_default(),
            ProviderKind::Codex,
            Some("gpt-5.6-sol".into()),
            None,
            cx,
        );

        let draft = state.selected_session().unwrap();
        assert_eq!(draft.meta.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(draft.meta.option_selections.len(), 1);
        assert_eq!(draft.meta.option_selections[0].id, "reasoningEffort");
        assert_eq!(
            draft.meta.option_selections[0].value,
            serde_json::json!("max")
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
        .save_commands(ProviderKind::ClaudeCode, None, &commands)
        .unwrap();

    let state = TestClientState::new(SessionStore::open_at(root.clone()).unwrap());
    let seeded = state.cached_provider_commands(ProviderKind::ClaudeCode, None);
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

/// Settings → Providers env/home/launch-args must reach the spawn options,
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
    let opts = session_options(&meta, &settings, launch_env, None, None, None, None);
    assert_eq!(opts.binary_path, Some(PathBuf::from("/custom/claude")));
    assert_eq!(opts.extra_args, vec!["--chrome", "--verbose"]);
    assert_eq!(
        opts.launch_env.pairs(ProviderKind::ClaudeCode),
        vec![
            (
                "ANTHROPIC_BASE_URL".to_string(),
                "https://proxy.test".to_string()
            ),
            ("HOME".to_string(), "/tmp/claude-home".to_string()),
        ]
    );

    // Codex takes its home as CODEX_HOME; this profile has no launch arguments.
    let launch_env = LaunchEnv {
        env: Vec::new(),
        home: settings.provider(ProviderKind::Codex).home_path.clone(),
    };
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
    let opts = session_options(&meta, &settings, launch_env, None, None, None, None);
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
        None,
        None,
        None,
        None,
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
        None,
        None,
        None,
        None,
    );
    assert_eq!(opts.extra_args, vec!["--verbose", "--approve"]);
}

/// Sensitive env rows contribute their value from `secrets.json`, never from
/// settings.json (which stores an empty value for them).
#[test]
fn launch_env_merges_secrets_for_sensitive_rows() {
    let test_store = TestStore::new("tcode-env-test");
    let store = (*test_store).clone();
    let mut state = TestClientState::new(store);
    let mut settings = state.settings.clone();
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
        // A sensitive row whose secret was never saved contributes nothing.
        EnvVar {
            name: "UNSET_SECRET".into(),
            value: String::new(),
            sensitive: true,
        },
    ];
    state.settings = settings;
    state
        .settings_store
        .set_profile_secret(
            Settings::builtin_profile_id(ProviderKind::ClaudeCode),
            "ANTHROPIC_API_KEY",
            Some("sk-x"),
        )
        .unwrap();

    let profile_id = Settings::builtin_profile_id(ProviderKind::ClaudeCode);
    let env = launch_env_for_profile(
        &state.settings,
        profile_id,
        state.settings_store.profile_secrets(profile_id),
    )
    .env;
    assert_eq!(
        env,
        vec![
            ("PLAIN".to_string(), "visible".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "sk-x".to_string()),
        ]
    );
}

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
    settings.provider_mut(ProviderKind::ClaudeCode).env = vec![EnvVar {
        name: "ANTHROPIC_API_KEY".into(),
        value: String::new(),
        sensitive: true,
    }];
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

    // The profile's launch env carries the Kimi endpoint + its own key.
    let env = launch_env_for_profile(
        &state.settings,
        "klaude-kode",
        state.settings_store.profile_secrets("klaude-kode"),
    )
    .env;
    assert!(env.contains(&(
        "ANTHROPIC_BASE_URL".to_string(),
        "https://api.kimi.com/coding/".to_string()
    )));
    assert!(env.contains(&("ANTHROPIC_MODEL".to_string(), "k3[1m]".to_string())));
    assert!(env.contains(&("ANTHROPIC_API_KEY".to_string(), "sk-kimi".to_string())));

    // The built-in profile is untouched: official key, no third-party URL.
    let builtin_profile_id = Settings::builtin_profile_id(ProviderKind::ClaudeCode);
    let builtin = launch_env_for_profile(
        &state.settings,
        builtin_profile_id,
        state.settings_store.profile_secrets(builtin_profile_id),
    )
    .env;
    assert!(builtin.contains(&("ANTHROPIC_API_KEY".to_string(), "sk-official".to_string())));
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
    let launch_env = session_launch_env(&state.settings, &state.settings_store, &meta);
    assert!(
        launch_env
            .env
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://api.kimi.com/coding/")
    );
    let opts = session_options(&meta, &state.settings, launch_env, None, None, None, None);
    assert_eq!(opts.binary_path, Some(PathBuf::from("/opt/kimi/claude")));
}

#[test]
fn session_launch_preserves_approval_policy_and_scopes_mcp_registrations() {
    let registration = |name: &str, port| agent::McpRegistration {
        name: name.into(),
        url: format!("http://127.0.0.1:{port}/mcp"),
        bearer_token: format!("{name}-token"),
    };
    let preview = registration(agent::McpRegistration::SERVER_NAME_PREVIEW, 7);
    let orchestrate = registration(agent::McpRegistration::SERVER_NAME_ORCHESTRATE, 8);
    let report = registration("child-report", 9);
    let computer = registration(agent::McpRegistration::SERVER_NAME_COMPUTER_USE, 10);
    for (provider, preview_supported) in [
        (ProviderKind::ClaudeCode, true),
        (ProviderKind::Codex, true),
        (ProviderKind::Pi, false),
    ] {
        for native_approvals in [false, true] {
            let mut settings = Settings::default();
            settings.provider_mut(provider).pi.native_approvals = native_approvals;
            for mode in [
                ApprovalMode::Supervised,
                ApprovalMode::AutoAcceptEdits,
                ApprovalMode::ReadOnly,
                ApprovalMode::FullAccess,
            ] {
                for (lead, child, computer_enabled) in [
                    (false, false, false),
                    (true, false, false),
                    (false, true, true),
                    (true, true, true),
                    (false, false, true),
                ] {
                    settings.computer_use.enabled = computer_enabled;
                    let mut meta = SessionMeta::new(provider, PathBuf::from("/x"), None);
                    meta.approval_mode = mode;
                    meta.orchestrate_enabled = lead;
                    meta.parent_session_id = child.then(|| "parent".into());
                    let options = session_options(
                        &meta,
                        &settings,
                        LaunchEnv::default(),
                        Some(preview.clone()),
                        Some(orchestrate.clone()),
                        Some(report.clone()),
                        Some(computer.clone()),
                    );
                    let expected_mode = match (provider, native_approvals, mode) {
                        (
                            ProviderKind::Pi,
                            false,
                            ApprovalMode::Supervised | ApprovalMode::AutoAcceptEdits,
                        ) => ApprovalMode::FullAccess,
                        _ => mode,
                    };
                    assert_eq!(
                        options.approval_mode, expected_mode,
                        "{provider:?}, native approvals {native_approvals}, {mode:?}"
                    );
                    assert_eq!(
                        meta.approval_mode, mode,
                        "launch must not rewrite saved intent"
                    );
                    let mut expected = Vec::new();
                    for (enabled, reg) in [
                        (preview_supported, &preview),
                        (lead, &orchestrate),
                        (child, &report),
                        (computer_enabled, &computer),
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
    }
}

#[test]
fn child_meta_links_parent_project_and_maps_effort() {
    let mut parent = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/p"), None);
    parent.id = "parent".into();
    parent.project_id = Some("project".into());
    let child = build_child_meta(
        &parent,
        ProviderKind::Codex,
        Some("gpt-test".into()),
        Some("high".into()),
        true,
        Some("work-codex".into()),
        ApprovalMode::AutoAcceptEdits,
        PathBuf::from("/p/sub"),
        true,
        Some(2400),
    );
    assert_eq!(child.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(child.project_id.as_deref(), Some("project"));
    assert_eq!(child.model.as_deref(), Some("gpt-test"));
    assert_eq!(child.profile_id.as_deref(), Some("work-codex"));
    assert_eq!(child.approval_mode, ApprovalMode::AutoAcceptEdits);
    assert!(child.archive_on_complete);
    assert_eq!(child.result_max_chars, Some(2400));
    assert_eq!(child.option_selections.len(), 2);
    assert_eq!(child.option_selections[0].id, "reasoningEffort");
    assert_eq!(child.option_selections[0].value, serde_json::json!("high"));
    assert_eq!(child.option_selections[1].id, "serviceTier");
    assert_eq!(child.option_selections[1].value, serde_json::json!("fast"));

    let claude = build_child_meta(
        &parent,
        ProviderKind::ClaudeCode,
        None,
        None,
        true,
        None,
        ApprovalMode::FullAccess,
        PathBuf::from("/p"),
        false,
        None,
    );
    assert_eq!(claude.option_selections.len(), 1);
    assert_eq!(claude.option_selections[0].id, "fastMode");
    assert_eq!(claude.option_selections[0].value, serde_json::json!(true));

    let pi = build_child_meta(
        &parent,
        ProviderKind::Pi,
        None,
        None,
        true,
        None,
        ApprovalMode::FullAccess,
        PathBuf::from("/p"),
        false,
        None,
    );
    assert!(pi.option_selections.is_empty());
}

#[test]
fn callbacks_preserve_reports_bound_fallback_output_and_describe_completion() {
    for (output, cap, expected_tail, truncated) in [
        (String::new(), None, "(no assistant output)".into(), false),
        ("done".into(), None, "done".into(), false),
        ("界".repeat(1200), None, "界".repeat(1200), false),
        ("界".repeat(1201), None, "界".repeat(600), true),
        ("start".repeat(1000) + "尾", Some(1), "尾".into(), true),
        ("界".repeat(5000), Some(300), "界".repeat(300), true),
        ("界".repeat(5000), Some(0), "界".repeat(5000), false),
    ] {
        let text = assemble_callback_text(
            "child",
            "Title",
            TurnStatus::Completed,
            &output,
            None,
            None,
            cap,
            false,
        );
        assert!(text.starts_with("[orchestrate] thread child (\"Title\") completed.\n"));
        assert!(!text.contains("tokens:"));
        assert_eq!(text.contains("Final output tail"), truncated);
        assert_eq!(text.lines().last().unwrap(), expected_tail);
    }
    let final_message = "f".repeat(3000);
    for (report, supplemented) in [
        ("R".repeat(199), true),
        ("R".repeat(200), false),
        ("R".repeat(5000), false),
    ] {
        let text = assemble_callback_text(
            "child",
            "Title",
            TurnStatus::Completed,
            &final_message,
            Some(&report),
            None,
            Some(300),
            false,
        );
        assert!(text.contains(&format!("Result (reported via report_result):\n{report}")));
        assert_eq!(text.contains("The report is brief"), supplemented);
        if supplemented {
            assert!(text.contains("Final output tail (3000 chars total"));
            assert_eq!(text.lines().last().unwrap(), "f".repeat(300));
        } else {
            assert!(text.ends_with(&report));
            assert!(!text.contains("Final output tail"));
            assert!(!text.contains(&"f".repeat(300)));
        }
    }
    let blank_report = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        "final message",
        Some("  \n"),
        None,
        None,
        false,
    );
    assert!(blank_report.ends_with("\nfinal message"));
    let short_output = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        "ok",
        Some("complete"),
        None,
        None,
        false,
    );
    assert!(short_output.ends_with("Result (reported via report_result):\ncomplete"));
    assert!(!short_output.contains("The report is brief"));

    let usage = agent::TokenUsage {
        input_tokens: Some(100),
        cached_input_tokens: Some(25),
        output_tokens: Some(40),
        total_processed_tokens: Some(165),
        ..Default::default()
    };
    for (status, archived, description) in [
        (TurnStatus::Completed, false, "completed"),
        (
            TurnStatus::Completed,
            true,
            "completed (auto-archived; send revives it)",
        ),
        (TurnStatus::Failed, false, "failed"),
        (TurnStatus::Interrupted, true, "failed"),
    ] {
        assert_eq!(
            assemble_callback_text(
                "child",
                "Title",
                status,
                "done",
                None,
                Some(&usage),
                None,
                archived
            ),
            format!(
                "[orchestrate] thread child (\"Title\") {description}. tokens: input 100 (+25 cached), output 40, total 165.\ndone"
            )
        );
    }
}

#[test]
fn dispatched_brief_carries_report_contract_footer() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-brief-footer-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    state.update(cx, |state, cx| {
        let parent = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
        let parent_id = parent.id.clone();
        state.sessions.push(parent);
        let child_id = state
            .create_child_session(
                &parent_id,
                ProviderKind::Codex,
                Some("gpt-test".into()),
                None,
                false,
                None,
                ApprovalMode::FullAccess,
                "Child".into(),
                None,
                "Inspect the workspace".into(),
                true,
                None,
                cx,
            )
            .unwrap();
        let queued = state.resident(&child_id).unwrap().queue[0].text.clone();
        assert!(queued.starts_with("Inspect the workspace"));
        assert!(queued.contains("report_result"), "brief: {queued}");
    });
}

#[test]
fn terminal_callback_archives_only_when_requested() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-callback-archive-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (parent_commands, _parent_receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        for (id, archive_on_complete, status) in [
            ("auto", true, TurnStatus::Completed),
            ("keep", false, TurnStatus::Completed),
            ("retry", true, TurnStatus::Failed),
        ] {
            let (commands, _receiver) = smol::channel::unbounded();
            let mut child = live_session(ProviderKind::Codex, commands);
            child.meta.id = id.into();
            child.meta.parent_session_id = Some("parent".into());
            child.meta.archive_on_complete = archive_on_complete;
            child.turn_in_flight = true;
            state.sessions.push(child.meta.clone());
            state.residents.parked.insert(child.meta.id.clone(), child);

            state.on_event(id, persisted_assistant_event("done"), cx);
            state.on_event(
                id,
                AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{id}"),
                    status,
                    usage: None,
                },
                cx,
            );
        }
    });

    cx.run_until(|state| {
        ["auto", "keep", "retry"]
            .iter()
            .all(|id| state.callback_last_turn.contains_key(*id))
    });

    state.read(|state| {
        assert!(
            state.find_meta("auto").unwrap().archived_at.is_some(),
            "archive_on_complete child should be archived after callback delivery"
        );
        assert!(
            state.find_meta("keep").unwrap().archived_at.is_none(),
            "control child should remain unarchived"
        );
        assert!(
            state.find_meta("retry").unwrap().archived_at.is_none(),
            "failed child should stay visible for retries"
        );
    });
}

#[test]
fn reported_result_reaches_parent_and_fallback_covers_silent_children() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-report-result-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        for id in ["reporter", "silent"] {
            let (commands, _receiver) = smol::channel::unbounded();
            let mut child = live_session(ProviderKind::Codex, commands);
            child.meta.id = id.into();
            child.meta.parent_session_id = Some("parent".into());
            child.meta.archive_on_complete = false;
            child.turn_in_flight = true;
            state.sessions.push(child.meta.clone());
            state.residents.parked.insert(child.meta.id.clone(), child);
        }

        // A report from a session that is not an orchestrated child is refused.
        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::ReportResult {
                child_id: "parent".into(),
                text: "nope".into(),
            },
            reply,
            cx,
        );
        assert!(response.try_recv().unwrap().is_err());

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::ReportResult {
                child_id: "reporter".into(),
                text: "the full reported RESULT".into(),
            },
            reply,
            cx,
        );
        assert!(response.try_recv().unwrap().is_ok());

        for id in ["reporter", "silent"] {
            state.on_event(id, persisted_assistant_event("last message"), cx);
            state.on_event(
                id,
                AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{id}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
                cx,
            );
        }
    });

    cx.run_until(|state| {
        ["reporter", "silent"]
            .iter()
            .all(|id| state.callback_last_turn.contains_key(*id))
    });

    let mut callbacks = Vec::new();
    while let Ok(command) = parent_receiver.try_recv() {
        if let SessionCommand::Steer { text, .. } = command {
            callbacks.push(text);
        }
    }
    assert_eq!(callbacks.len(), 2, "one callback per completed child");
    let reported = callbacks
        .iter()
        .find(|text| text.contains("thread reporter"))
        .unwrap();
    assert!(reported.contains("Result (reported via report_result):\nthe full reported RESULT"));
    assert!(!reported.contains("last message"));
    let silent = callbacks
        .iter()
        .find(|text| text.contains("thread silent"))
        .unwrap();
    assert!(silent.ends_with("\nlast message"));

    state.read(|state| {
        assert!(
            state.child_reported_results.is_empty(),
            "delivery should consume the stored report"
        );
    });
}

#[test]
fn orchestrate_send_reactivates_the_child_and_its_settled_parent() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-send-unarchive-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, _receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut child = live_session(ProviderKind::Codex, commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        child.meta.archived_at = Some(1);
        child.meta.settled_at = Some(1);
        let mut parent = SessionMeta::new(ProviderKind::Codex, test_store.root().clone(), None);
        parent.id = "parent".into();
        parent.settled_at = Some(1);
        state.sessions.push(parent);
        child.turn_in_flight = true;
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Send {
                parent_id: "parent".into(),
                thread_id: "child".into(),
                message: "retry with the failing test".into(),
                fast: None,
            },
            reply,
            cx,
        );
        assert!(response.try_recv().unwrap().is_ok());
        assert!(state.find_meta("child").unwrap().settled_at.is_none());
        assert!(state.find_meta("parent").unwrap().settled_at.is_none());
        assert!(
            state.find_meta("child").unwrap().archived_at.is_none(),
            "send should revive an archived child"
        );
    });
}

#[test]
fn orchestrate_send_fast_switch_persists_and_schedules_restart() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-send-fast-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, _receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut child = live_session(ProviderKind::Codex, commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        child.turn_in_flight = true;
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        let send = |state: &mut AppState, cx: &mut HostCx, fast: Option<bool>| {
            let (reply, response) = smol::channel::bounded(1);
            state.handle_orchestrate_op(
                orchestrate_mcp::OrchestrateOp::Send {
                    parent_id: "parent".into(),
                    thread_id: "child".into(),
                    message: "carry on".into(),
                    fast,
                },
                reply,
                cx,
            );
            assert!(response.try_recv().unwrap().is_ok());
        };
        let is_fast = |state: &AppState| {
            let selected = |selections: &[OptionSelection]| {
                selections
                    .iter()
                    .any(|selection| selection.id == "serviceTier" && selection.value == "fast")
            };
            let resident = selected(&state.resident("child").unwrap().meta.option_selections);
            let indexed = selected(&state.find_meta("child").unwrap().option_selections);
            assert_eq!(resident, indexed, "resident and index must agree");
            resident
        };

        send(state, cx, None);
        assert!(!is_fast(state), "omitted: unchanged");
        assert!(
            !state
                .resident("child")
                .unwrap()
                .options_changed_while_live()
        );

        send(state, cx, Some(true));
        assert!(is_fast(state), "fast on");
        assert!(
            state
                .resident("child")
                .unwrap()
                .options_changed_while_live(),
            "a live child must restart before its next turn"
        );

        send(state, cx, Some(false));
        assert!(!is_fast(state), "fast off");
    });
}

#[test]
fn orchestrate_archive_is_batch_atomic_and_parent_scoped() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-archive-op-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        for (id, parent_id) in [
            ("child-a", "parent"),
            ("child-b", "parent"),
            ("foreign", "other-parent"),
        ] {
            let mut meta =
                SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
            meta.id = id.into();
            meta.parent_session_id = Some(parent_id.into());
            state.sessions.push(meta);
        }

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Archive {
                parent_id: "parent".into(),
                thread_ids: vec!["child-a".into(), "missing".into(), "foreign".into()],
            },
            reply,
            cx,
        );
        let error = response.try_recv().unwrap().unwrap_err();
        assert!(error.contains("missing"));
        assert!(error.contains("foreign"));
        assert!(state.find_meta("child-a").unwrap().archived_at.is_none());
        assert!(state.find_meta("child-b").unwrap().archived_at.is_none());

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Archive {
                parent_id: "parent".into(),
                thread_ids: vec!["child-a".into(), "child-b".into()],
            },
            reply,
            cx,
        );
        assert_eq!(
            response.try_recv().unwrap().unwrap(),
            serde_json::json!({
                "ok": true,
                "archived": 2,
                "thread_ids": ["child-a", "child-b"],
            })
        );
        assert!(state.find_meta("child-a").unwrap().archived_at.is_some());
        assert!(state.find_meta("child-b").unwrap().archived_at.is_some());
        let archived = state.find_meta("child-a").unwrap();
        assert_eq!(
            state.child_status_json(&archived, &Timeline::default())["archived"],
            true
        );
    });
}

#[test]
fn child_approval_policy_distinguishes_report_tools_and_deduplicates_notices() {
    for mode in [
        ChildApprovalMode::Orchestrator,
        ChildApprovalMode::Manual,
        ChildApprovalMode::AlwaysAllow,
    ] {
        for (provider, tool, is_report) in [
            (ProviderKind::Codex, None, false),
            (
                ProviderKind::ClaudeCode,
                Some("mcp__tcode_report__report_result"),
                true,
            ),
            (
                ProviderKind::ClaudeCode,
                Some("mcp__evil_tcode_report__delete_files"),
                false,
            ),
            (
                ProviderKind::ClaudeCode,
                Some("mcp__tcode_report__delete_files"),
                false,
            ),
            (
                ProviderKind::Pi,
                Some("mcp__tcode_report__report_result"),
                false,
            ),
            (
                ProviderKind::Acp,
                Some("mcp__tcode_report__report_result"),
                false,
            ),
        ] {
            let cx = &mut TestAppContext::default();
            let store = TestStore::new("tcode-child-approval-policy");
            let state = cx.new_entity(TestClientState::new((*store).clone()));
            let (parent_commands, parent_receiver) = smol::channel::unbounded();
            let (child_commands, child_receiver) = smol::channel::unbounded();
            state.update(cx, |state, cx| {
                state.settings.orchestrate.child_approval = mode;
                let mut parent = live_session(ProviderKind::Codex, parent_commands);
                parent.meta.id = "parent".into();
                parent.turn_in_flight = true;
                state.residents.parked.insert("parent".into(), parent);
                let mut child = live_session(provider, child_commands);
                child.meta.id = "child".into();
                child.meta.title = "Review".into();
                child.meta.parent_session_id = Some("parent".into());
                state.sessions.push(child.meta.clone());
                state.residents.parked.insert("child".into(), child);
                let (kind, summary) = if let Some(name) = tool {
                    (
                        agent::ApprovalKind::ToolUse {
                            name: name.into(),
                            input: serde_json::json!({"text": "report"}),
                            detail: name.into(),
                        },
                        format!("tool `{name}`"),
                    )
                } else {
                    (
                        agent::ApprovalKind::ExecCommand {
                            command: "touch blocked".into(),
                            cwd: None,
                            reason: None,
                        },
                        "command `touch blocked`".into(),
                    )
                };
                let request = agent::ApprovalRequest {
                    id: "approval".into(),
                    turn_id: None,
                    kind,
                    options: Vec::new(),
                };
                state.on_event("child", AgentEvent::ApprovalRequested(request.clone()), cx);
                if is_report || mode == ChildApprovalMode::AlwaysAllow {
                    let expected_decision = if is_report {
                        ApprovalDecision::Approve
                    } else {
                        ApprovalDecision::ApproveForSession
                    };
                    assert!(
                        matches!(child_receiver.try_recv(), Ok(SessionCommand::RespondApproval {
                        request_id, decision,
                    }) if request_id == "approval" && decision == expected_decision),
                        "{provider:?} {mode:?} {tool:?}"
                    );
                    assert!(parent_receiver.try_recv().is_err());
                    assert!(!state.has_approval("child"));
                } else {
                    assert!(
                        child_receiver.try_recv().is_err(),
                        "unexpected automatic approval: {provider:?} {mode:?} {tool:?}"
                    );
                    state.on_event("child", AgentEvent::ApprovalRequested(request), cx);
                    let SessionCommand::Steer { text, .. } = parent_receiver.try_recv().unwrap()
                    else {
                        panic!("approval notice must steer the parent");
                    };
                    let prefix = format!(
                        "[orchestrate] thread child (\"Review\") is waiting for approval: {summary}"
                    );
                    if mode == ChildApprovalMode::Manual {
                        assert_eq!(text, format!("{prefix}."));
                    } else {
                        assert!(text.starts_with(&format!("{prefix} (request_id: approval).")));
                        assert!(text.contains("decide with the approve tool"));
                    }
                    assert!(
                        parent_receiver.try_recv().is_err(),
                        "duplicate provider request produced another notice"
                    );
                    let status = state.child_status_json(&state.sessions[0], &Timeline::default());
                    assert_eq!(status["waiting_approval"], summary);
                    assert_eq!(status["approval_request_id"], "approval");
                }
            });
        }
    }
}

#[test]
fn orchestrate_approve_routes_decisions_and_validates_scope() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-approve-op-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut child = live_session(ProviderKind::Codex, commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);
        state.record_approval_event(
            "child",
            &AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval-op".into(),
                turn_id: None,
                kind: agent::ApprovalKind::ExecCommand {
                    command: "cargo test".into(),
                    cwd: None,
                    reason: None,
                },
                options: Vec::new(),
            }),
        );

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Approve {
                parent_id: "parent".into(),
                thread_id: "child".into(),
                request_id: None,
                decision: " APPROVE ".into(),
            },
            reply,
            cx,
        );
        let result = response.try_recv().unwrap().unwrap();
        assert_eq!(
            result,
            serde_json::json!({ "ok": true, "request_id": "approval-op" })
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::RespondApproval {
                request_id,
                decision: ApprovalDecision::Approve,
            }) if request_id == "approval-op"
        ));

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Approve {
                parent_id: "parent".into(),
                thread_id: "child".into(),
                request_id: Some("missing".into()),
                decision: "deny".into(),
            },
            reply,
            cx,
        );
        let unknown_request = response.try_recv().unwrap().unwrap_err();
        assert_eq!(unknown_request, "no pending approval with that request_id");

        // The successful response above clears the request immediately. Seed a
        // fresh provider request before independently exercising validation.
        state.record_approval_event(
            "child",
            &AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval-op".into(),
                turn_id: None,
                kind: agent::ApprovalKind::ExecCommand {
                    command: "cargo test".into(),
                    cwd: None,
                    reason: None,
                },
                options: Vec::new(),
            }),
        );

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Approve {
                parent_id: "parent".into(),
                thread_id: "child".into(),
                request_id: Some("approval-op".into()),
                decision: "later".into(),
            },
            reply,
            cx,
        );
        let bad_decision = response.try_recv().unwrap().unwrap_err();
        assert_eq!(
            bad_decision,
            "unknown decision: later; expected approve, approve_for_session, or deny"
        );

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Approve {
                parent_id: "other-parent".into(),
                thread_id: "child".into(),
                request_id: Some("approval-op".into()),
                decision: "deny".into(),
            },
            reply,
            cx,
        );
        let non_child = response.try_recv().unwrap().unwrap_err();
        assert_eq!(non_child, "unknown thread or not a child of this parent");
    });
}

#[test]
fn final_assistant_message_joins_all_blocks_of_the_final_output() {
    let timeline = Timeline::fold_events([
        AgentEvent::ItemCompleted(ThreadItem {
            id: "preamble".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "Earlier tool preamble.".into(),
            },
        }),
        AgentEvent::ItemCompleted(ThreadItem {
            id: "reasoning".into(),
            parent_item_id: None,
            content: ItemContent::Reasoning {
                text: "private reasoning".into(),
            },
        }),
        AgentEvent::ItemCompleted(ThreadItem {
            id: "final-1".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "Complete ".into(),
            },
        }),
        AgentEvent::ItemCompleted(ThreadItem {
            id: "final-2".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "answer.".into(),
            },
        }),
    ]);

    assert_eq!(final_assistant_message(&timeline), "Complete answer.");
}

#[test]
fn resident_background_child_result_uses_completed_live_timeline() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-resident-result-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let report = format!("Complete child report:\n{}", "full detail ".repeat(80));

    state.update(cx, |state, cx| {
        let mut parent = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
        parent.id = "parent".into();

        let (commands, _receiver) = smol::channel::unbounded();
        let mut child = live_session(ProviderKind::Codex, commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some(parent.id.clone());
        child.turn_in_flight = true;

        state.sessions.push(parent);
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        state.on_event("child", persisted_assistant_event(&report), cx);
        state.on_event(
            "child",
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );

        let child = state.residents.parked.get("child").unwrap();
        assert!(
            child.idle_since.is_some(),
            "completed child must remain resident in background"
        );
        assert!(!child.turn_in_flight);

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Result {
                parent_id: "parent".into(),
                thread_id: "child".into(),
            },
            reply,
            cx,
        );
        let result = response.try_recv().unwrap().unwrap();
        assert_eq!(result["state"], "completed");
        assert_eq!(result["final_message"], report);
    });
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
        let timeline = Timeline::fold_events(state.store.read_events("parent"));
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
        state.install_selected(active);

        state.steer("active", "redirect".into(), Vec::new(), cx);
        let SessionCommand::Steer { request_id, .. } = receiver.try_recv().unwrap() else {
            panic!("user steer command missing")
        };
        let active = state.selected_session().unwrap();
        assert!(active.timeline.entries.iter().any(|entry| matches!(
            &entry.content,
            EntryContent::Steer {
                text,
                status: tcode_core::session::SteeringStatus::Pending,
                ..
            } if entry.id == request_id && text == "redirect"
        )));

        let active = state.selected_session_mut().unwrap();
        let first = active.push_queued("first".into(), Vec::new());
        let queued_id = active.push_queued("queued redirect".into(), Vec::new());
        let third = active.push_queued("third".into(), Vec::new());
        state.steer_queued("active", queued_id, cx);
        let SessionCommand::Steer { request_id, .. } = receiver.try_recv().unwrap() else {
            panic!("queue-to-steer command missing")
        };
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
        let scheduled_id = status.queued_messages[0].id;

        state.steer_queued("scheduled-active", scheduled_id, cx);
        let status = state.session_status_snapshot("scheduled-active").unwrap();
        assert_eq!(status.queued_messages.len(), 1);
        assert_eq!(status.queued_messages[0].fire_at_unix_secs, None);
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
fn implement_plan_waits_for_pending_relay_without_mutating_state() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-plan-relay-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "plan-relay".into();
        active.meta.interaction_mode = InteractionMode::Plan;
        active.pending_relay = Some(PendingRelay {
            from_provider: ProviderKind::ClaudeCode,
            from_model: Some("opus".into()),
            from_profile: None,
        });
        active.timeline.apply_at(
            None,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "user-1".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "make a plan".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
        );
        active.timeline.apply_at(
            None,
            &AgentEvent::ProposedPlan {
                item_id: "plan-1".into(),
                markdown: "# Plan".into(),
            },
        );
        active.timeline.apply_at(
            None,
            &AgentEvent::TurnCompleted {
                turn_id: "plan-turn".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        );
        state.install_selected(active);

        state.implement_plan("plan-relay", cx);

        let active = state.selected_session().unwrap();
        assert_eq!(active.meta.interaction_mode, InteractionMode::Plan);
        assert!(active.timeline.plan_ready().is_some());
        assert!(receiver.try_recv().is_err());
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

#[test]
fn queued_turns_keep_the_interaction_mode_selected_at_submit_time() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    active.turn_in_flight = true;
    active.meta.interaction_mode = InteractionMode::Plan;
    active.push_queued("plan turn".into(), Vec::new());
    active.meta.interaction_mode = InteractionMode::Build;
    active.turn_in_flight = false;

    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let first_delivery = match receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id,
            options: Some(options),
            ..
        }) => {
            assert_eq!(options.interaction_mode, Some(InteractionMode::Plan));
            delivery_id
        }
        other => panic!("expected queued Plan turn, got {other:?}"),
    };
    active.accept_turn_delivery(first_delivery).unwrap();
    active.turn_in_flight = false;

    active.meta.interaction_mode = InteractionMode::Build;
    active.push_queued("build turn".into(), Vec::new());
    active.meta.interaction_mode = InteractionMode::Plan;
    assert_eq!(active.dispatch_next_pending(), Ok(true));
    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionCommand::SendTurn {
            options: Some(TurnOptions {
                interaction_mode: Some(InteractionMode::Build),
                ..
            }),
            ..
        })
    ));
}

/// A live session with `provider`, nothing queued, no turn in flight.
fn live_session(
    provider: ProviderKind,
    commands: smol::channel::Sender<SessionCommand>,
) -> ActiveSession {
    ActiveSession {
        runtime: Runtime::Live(commands),
        live_approval_mode: Some(ApprovalMode::default()),
        ..ActiveSession::new(
            SessionMeta::new(provider, PathBuf::from("/tmp/project"), None),
            false,
            Vec::new(),
        )
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
    assert_eq!(active.queue[0].context_window_changed, Some(500_000));

    active.runtime = Runtime::Idle;
    active.push_scheduled("idle".into(), Vec::new(), SystemTime::now());
    assert_eq!(active.queue[1].context_window_changed, None);
}

#[test]
fn native_rewind_waits_for_provider_confirmation_before_pruning() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-native-rewind-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::ClaudeCode, commands);
        active.meta.id = "claude-session".into();
        for index in 1..=2 {
            active.timeline.apply_at(
                Some(index * 10),
                &AgentEvent::TurnStarted {
                    turn_id: format!("turn-{index}"),
                },
            );
            active.timeline.apply_at(
                Some(index * 10 + 1),
                &AgentEvent::TurnCheckpoint {
                    turn_id: format!("turn-{index}"),
                    checkpoint_id: format!("checkpoint-{index}"),
                },
            );
            active.timeline.apply_at(
                Some(index * 10 + 2),
                &AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{index}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            );
        }
        state.install_selected(active);
        state.rewind_turn("claude-session", 1, RewindMode::Conversation, cx);
        assert_eq!(state.selected_session().unwrap().timeline.turns.len(), 2);
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::Rewind {
                checkpoint_id,
                mode: RewindMode::Conversation,
            }) if checkpoint_id == "checkpoint-2"
        ));

        state.on_event(
            "claude-session",
            AgentEvent::RewindCompleted {
                checkpoint_id: "checkpoint-2".into(),
                mode: RewindMode::Conversation,
                prefill: Some("original prompt".into()),
            },
            cx,
        );
        assert_eq!(state.selected_session().unwrap().timeline.turns.len(), 1);
        assert!(!state.native_rewind_pending("claude-session"));
    });
    let mut serialized_prefill = None;
    while let Ok(line) = cx.outgoing_rx.try_recv() {
        let output = tcode_protocol::decode_host_line(&line).expect("decode host test output");
        if let HostMessage::Event(EventEnvelope {
            request_id: None,
            event: ServerEvent::NativeRewindPrefill { session_id, text },
            ..
        }) = output
            && session_id == "claude-session"
        {
            serialized_prefill = Some(text);
        }
    }
    assert_eq!(serialized_prefill.as_deref(), Some("original prompt"));
}

#[test]
fn turn_blocked_clears_active_session_queue_when_abort_on_model_fallback_is_enabled() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-turn-blocked-queue-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    state.update(cx, |state, cx| {
        state.settings.abort_on_model_fallback = true;
        let mut active = ActiveSession::new(
            SessionMeta::new(
                ProviderKind::ClaudeCode,
                PathBuf::from("/tmp/turn-blocked"),
                Some("claude-opus-test".into()),
            ),
            false,
            Vec::new(),
        );
        let session_id = active.meta.id.clone();
        active.push_queued("do not auto-send".into(), Vec::new());
        assert!(!active.queue.is_empty());
        state.install_selected(active);

        state.on_event(
            &session_id,
            AgentEvent::TurnBlocked {
                category: Some(agent::ClassifierCategory::Cyber),
                model: Some("claude-opus-test".into()),
                detail: "request blocked by classifier".into(),
            },
            cx,
        );

        assert!(state.selected_session().unwrap().queue.is_empty());
    });

    assert!(cx.drain_outgoing().iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope { request_id: None,
            topic: Topic::SessionStatus { .. },
            event: ServerEvent::ModelFallbackBlocked {
                category: Some(agent::ClassifierCategory::Cyber),
                model: Some(model),
                fallback_model: None,
                detail,
                ..
            },
        }) if model == "claude-opus-test" && detail == "request blocked by classifier"
    )));
}

#[test]
fn model_fallback_stops_active_session_when_abort_on_model_fallback_is_enabled() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-model-fallback-stop-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, receiver) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        state.settings.abort_on_model_fallback = true;
        let mut active = live_session(ProviderKind::ClaudeCode, commands);
        active.meta.model = Some("claude-fable-5-1".into());
        active.turn_in_flight = true;
        active.timeline.apply_at(
            None,
            &AgentEvent::TurnStarted {
                turn_id: "turn-fallback".into(),
            },
        );
        let session_id = active.meta.id.clone();
        active.push_queued("do not auto-send".into(), Vec::new());
        state.install_selected(active);

        state.on_event(
            &session_id,
            AgentEvent::ModelFallbackDetected {
                expected: "claude-fable-5-1".into(),
                actual: "claude-fable-5".into(),
                category: None,
                checkpoint_id: None,
                parent_tool_use_id: None,
            },
            cx,
        );

        let active = state.selected_session().unwrap();
        assert!(active.queue.is_empty());
        assert!(matches!(active.runtime, Runtime::Idle));
        assert!(!active.turn_in_flight);
        assert!(!active.timeline.turn_running);
        assert_eq!(
            active.timeline.last_turn_status,
            Some(TurnStatus::Interrupted)
        );
        assert!(active.timeline.turns.last().unwrap().end_ts.is_some());
    });

    assert!(matches!(receiver.try_recv(), Ok(SessionCommand::Shutdown)));
    let outgoing = cx.drain_outgoing();
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            topic: Topic::SessionEvents { .. },
            event: ServerEvent::SessionEvent(SessionEventRecord {
                event: AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Interrupted, .. },
                ..
            }),
            ..
        }) if turn_id == "turn-fallback"
    )));
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope { request_id: None,
            topic: Topic::SessionStatus { .. },
            event: ServerEvent::ModelFallbackBlocked {
                model: Some(model),
                fallback_model: Some(fallback_model),
                ..
            },
        }) if model == "claude-fable-5-1" && fallback_model == "claude-fable-5"
    )));
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

    // OpenCode and ACP have no steering method, so a steer must fall back
    // to the queue rather than silently vanish.
    let mut opencode = live_session(ProviderKind::OpenCode, commands.clone());
    opencode.turn_in_flight = true;
    assert_eq!(opencode.route(true), SendRouting::QueueUnsupported);

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

/// Steering must not disturb the turn bookkeeping: it joins the turn already
/// in flight, so no queue entry is consumed and no new turn is opened.
#[test]
fn steering_does_not_disturb_turn_accounting() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    active.turn_in_flight = true;
    active.push_queued("queued".into(), Vec::new());

    assert_eq!(
        active.steer_now("steer-1".into(), "steer me".into(), Vec::new()),
        Ok(())
    );

    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionCommand::Steer { request_id, text, .. })
            if request_id == "steer-1" && text == "steer me"
    ));
    // Still exactly one turn in flight, and the queue is untouched.
    assert!(active.turn_in_flight);
    assert_eq!(active.queue.len(), 1);
    assert_eq!(active.queue[0].text, "queued");
}

/// Ultrathink is per-send: it rides with the message it was armed for, not
/// with whatever happens to be dispatched later.
#[test]
fn ultrathink_rides_with_the_queued_message() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    active.turn_in_flight = true;
    active.pending_ultrathink = true;
    active.push_queued("deep".into(), Vec::new());
    // The flag is consumed by the message that was armed for it.
    assert!(!active.pending_ultrathink);
    active.push_queued("shallow".into(), Vec::new());

    active.turn_in_flight = false;
    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let first_delivery = match receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) if text == "Ultrathink:\ndeep" => delivery_id,
        other => panic!("expected Ultrathink SendTurn, got {other:?}"),
    };
    active.accept_turn_delivery(first_delivery).unwrap();
    active.turn_in_flight = false;
    assert_eq!(active.dispatch_next_pending(), Ok(true));
    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionCommand::SendTurn { text, .. }) if text == "shallow"
    ));
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
    assert!(text.contains("<conversation-transcript>\n# prior work"));
    assert!(text.contains("<new-user-message>\ncontinue here"));

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
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
    let mut active = ActiveSession {
        runtime: Runtime::Starting { generation: 2 },
        ..ActiveSession::new(meta, false, Vec::new())
    };

    assert!(!active.is_starting_generation(1));
    assert!(active.is_starting_generation(2));
    active.runtime = Runtime::Live(smol::channel::unbounded().0);
    assert!(!active.is_starting_generation(2));
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
        let (delivery_id, submitted_text) = match first_actor.try_recv() {
            Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) => (delivery_id, text),
            other => panic!("expected submitted SendTurn, got {other:?}"),
        };
        let active = state.selected_session().unwrap();
        assert_eq!(active.queue.len(), 1);
        assert_eq!(active.delivery_in_flight, Some(delivery_id));
        assert!(!state.store.read_events(&session_id).iter().any(|stored| {
            matches!(
                &stored.event,
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::UserMessage { text, .. },
                    ..
                }) if text == "survive the eof race"
            )
        }));

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
        let retried_delivery = match resumed_actor.try_recv() {
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
        assert!(active.idle_since.is_none());
        assert!(!state.residents.parked.contains_key("idle-resident"));
        assert!(actor.try_recv().is_err(), "re-adoption sent Shutdown");
    });
    cx.run_until_parked();
    assert_eq!(test_store.load_index()[0].updated_at, updated_at);
}

#[test]
fn subscribing_readopts_an_uncommitted_draft_before_its_idle_reaper() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-draft-subscription-readopt-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    state.update(cx, |state, cx| {
        let id = AppState::start_draft(state, "project".into(), PathBuf::from("/tmp"), cx);
        let subscription = tcode_protocol::Subscription {
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
        assert!(state.residents.live[&id].idle_since.is_none());
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
fn resident_idle_reaper_ignores_readopted_session() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-stale-timer-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "idle-resident".into();
    let meta = session.meta.clone();

    state.update(cx, |state, cx| {
        state.sessions.push(meta);
        state.install_selected(session);
        state.park_active(cx);
        let idle_since = state.residents.parked["idle-resident"].idle_since.unwrap();
        state.select_session("idle-resident", cx);
        // Fire the grace timer that parking armed, now that its session has
        // been re-adopted; the real timer would land identically but later.
        state.reap_idle_resident("idle-resident", idle_since, cx);

        let active = state.selected_session().unwrap();
        assert_eq!(active.meta.id, "idle-resident");
        assert!(matches!(active.runtime, Runtime::Live(_)));
        assert!(actor.try_recv().is_err(), "stale timer sent Shutdown");
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

    state.update(cx, |state, cx| {
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .binary_path = Some("/nonexistent/tcode-test-claude".into());
        state.install_selected(session);
        state.send_turn(
            "background-restart",
            "use the new model later".into(),
            Vec::new(),
            cx,
        );
        assert!(actor.try_recv().is_err());
        assert_eq!(state.selected_session().unwrap().queue.len(), 1);

        // Even an early background-task drain cannot restart the provider
        // before its self-invoked follow-up turn closes.
        state.on_event(
            "background-restart",
            AgentEvent::BackgroundTasksChanged { count: 0 },
            cx,
        );
        assert!(actor.try_recv().is_err());
        state.on_event(
            "background-restart",
            AgentEvent::TurnStarted {
                turn_id: "task-follow-up".into(),
            },
            cx,
        );
        state.on_event(
            "background-restart",
            AgentEvent::TurnCompleted {
                turn_id: "task-follow-up".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(matches!(actor.try_recv(), Ok(SessionCommand::Shutdown)));
        assert_eq!(state.selected_session().unwrap().queue.len(), 1);
    });
}

#[test]
fn model_switch_restarts_live_provider() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut meta = SessionMeta::new(
        ProviderKind::ClaudeCode,
        PathBuf::from("/tmp/project"),
        None,
    );
    meta.model = Some("sonnet".into());
    let mut active = ActiveSession {
        runtime: Runtime::Live(commands),
        // Process was started on "opus"; the user has since picked "sonnet".
        live_model: Some("opus".into()),
        queue: vec!["do it".into()],
        next_queue_id: 1,
        ..ActiveSession::new(meta, false, Vec::new())
    };

    assert!(active.model_changed_while_live());
    active.shutdown_to_idle();

    // Live provider is told to shut down and the runtime is back to Idle,
    // while the queued turn is preserved for the restarted process.
    assert!(matches!(receiver.try_recv(), Ok(SessionCommand::Shutdown)));
    assert!(matches!(active.runtime, Runtime::Idle));
    assert_eq!(active.queue, [QueuedMessage::from("do it")]);
    assert!(!active.model_changed_while_live());

    // No restart when the selected model matches the live one.
    active.runtime = Runtime::Live(smol::channel::unbounded().0);
    active.live_model = active.meta.model.clone();
    assert!(!active.model_changed_while_live());
}

#[test]
fn unread_requires_a_visit_before_the_latest_update() {
    let test_store = TestStore::new("tcode-unread-watermark-test");
    let mut state = TestClientState::new((*test_store).clone());
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/p"), None);
    meta.updated_at = 100;
    let id = meta.id.clone();
    state.sessions.push(meta);

    assert!(!state.session_unread(&id));
    state.settings.last_visited.insert(id.clone(), 50);
    assert!(state.session_unread(&id));
    state.settings.last_visited.insert(id.clone(), 100);
    assert!(!state.session_unread(&id));
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
    let state = cx.new_entity(TestClientState::new(store));

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
        assert!(fork.pending_fork);
        assert_eq!(
            fork.resume_cursor.as_ref().unwrap().0["thread_id"],
            "native-source"
        );
        assert_eq!(fork.cwd, source.cwd);
        assert_eq!(fork.worktree, None);
        assert!(!active.timeline.turn_running);
        assert_eq!(state.store.read_events(&fork.id).len(), 1);
        assert_eq!(
            std::fs::read(root.join(format!("{}.jsonl", fork.id))).unwrap(),
            std::fs::read(root.join(format!("{}.jsonl", source.id))).unwrap()
        );
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
    });
    cx.run_until_parked();

    let fresh = SessionStore::open_at(test_store.root().clone()).unwrap();
    assert_eq!(
        fresh
            .load_index()
            .iter()
            .find(|meta| meta.id == id)
            .unwrap()
            .title,
        "persisted by writer"
    );
    let events = fresh.read_events(&id);
    let texts: Vec<_> = events
        .iter()
        .filter_map(|stored| match &stored.event {
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text },
                ..
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["first", "second"]);
    let settings = SettingsStore::new(test_store.root().clone());
    assert_eq!(
        settings
            .profile_secrets("profile")
            .get("ANTHROPIC_API_KEY")
            .map(String::as_str),
        Some("writer-secret")
    );
}

/// A project's unsent draft is keyed by the project, so deleting the project
/// drops the draft's parked terminal workspace and its persisted terminal
/// preferences instead of leaving them behind forever.
#[test]
fn deleting_a_project_clears_its_drafts_terminal_state() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-delete-project-draft");
    let root = test_store.root().clone();
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    state.update(cx, |state, cx| {
        state.start_draft("doomed".into(), root.clone(), cx);
        let draft_id = state.selected.clone().expect("draft selected");
        state.host.set_terminal_height(&draft_id, 320., cx);
        state.host.park_active(&draft_id, cx);
    });
    let destination = ConversationDestination::ProjectDraft("doomed".into());
    state.read(|state| {
        assert!(state.host.terminal_workspaces.contains_key(&destination));
        assert!(
            state
                .host
                .terminal_preferences
                .contains_key(&destination.preference_key())
        );
    });

    state.update(cx, |state, cx| state.host.delete_project("doomed", cx));
    cx.run_until_parked();

    state.read(|state| {
        assert!(
            !state.host.terminal_workspaces.contains_key(&destination),
            "the deleted project's draft kept its terminal workspace"
        );
        assert!(
            state.host.terminal_preferences.is_empty(),
            "the deleted project's draft kept terminal preferences: {:?}",
            state.host.terminal_preferences
        );
    });
}

#[test]
fn terminal_open_installs_after_executor_pump_and_preserves_cwd_override() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-terminal-open");
    let root = test_store.root().clone();
    let override_cwd = root.join("override");
    std::fs::create_dir_all(&override_cwd).unwrap();
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, _| {
        state.install_selected(AppState::build_draft_session(
            "terminal-project".into(),
            root.clone(),
            ProviderKind::Codex,
            None,
            None,
            Vec::new(),
        ));
    });
    term::Terminal::with_spawn_cwd(override_cwd.clone(), || {
        state.update(cx, |state, cx| {
            state
                .host
                .open_terminal_panel(state.selected.as_deref().unwrap_or_default(), cx)
        });
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
        assert_eq!(workspace.terminals[0].terminal.cwd(), override_cwd);
    });
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
    session.live_approval_mode = Some(session.meta.approval_mode);
    session.live_option_selections = session.meta.option_selections.clone();
    (session, receiver)
}

fn persisted_assistant_event(text: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: format!("item-{text}"),
        parent_item_id: None,
        content: ItemContent::AssistantMessage { text: text.into() },
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
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "persisted cold output")
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
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "while parked")
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
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "only session B")
        ));
        assert!(!active.timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "only session A")
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
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "before load")
        ));
        assert!(timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "raced append")
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
        let replayed = Timeline::fold_events(state.store.read_events(&id_b));
        assert!(replayed.entries.iter().any(
            |e| matches!(&e.content, EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "second message")
        ));
    });

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
        state.sessions = state.store.load_index();
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
            state.install_selected(session);
            let active_answer = state.turn_running_for(&id);
            assert_eq!(
                state.working_sessions_count(),
                usize::from(expected),
                "{label}"
            );

            let parked = state.take_selected().unwrap();
            state.residents.parked.insert(id.clone(), parked);
            let parked_answer = state.turn_running_for(&id);
            assert_eq!(
                state.working_sessions_count(),
                usize::from(expected),
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
        state.sessions = state.store.load_index();
        state.install_selected(session);
        state.send_turn(&id_a, "run the long migration".into(), Vec::new(), cx);
        state.send_turn(&id_a, "queued follow-up".into(), Vec::new(), cx);
        let first_delivery = match commands_a.try_recv() {
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
            commands_a.try_recv().is_err(),
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
        let follow_up_delivery = match commands_a.try_recv() {
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
            EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "Migration step 1 done."
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
        state.sessions = state.store.load_index();
        state.install_selected(session);
        state.send_turn(&id, "one last thing".into(), Vec::new(), cx);
        let delivery_id = match commands.try_recv() {
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
        assert!(commands.try_recv().is_err(), "drain sent Shutdown");
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

    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn plan_workspace_save_completes_after_background_executor_runs() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-plan-save-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-plan-save-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        state.start_draft("plan-project".into(), cwd.clone(), cx);
        state.host.save_plan_to_workspace(
            state.selected.as_deref().unwrap_or_default(),
            "# Saved plan".into(),
            cx,
        );
        // No "not written yet" assertion here: the write runs on the global
        // smol pool, whose threads are concurrent with this update, so any
        // file-existence check races them (flaked on fast Windows runners
        // both inside and after this update).
    });

    cx.run_until(|_| {
        std::fs::read_to_string(cwd.join("PLAN-1.md")).is_ok_and(|text| text == "# Saved plan")
    });
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

#[test]
fn orchestrate_dispatch_fast_override_beats_profile_setting() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-dispatch-fast-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let parent = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
    let parent_id = parent.id.clone();

    // Fast mode belongs to the model, independent of reasoning effort.
    state.update(cx, |state, _| {
        state.sessions.push(parent);
        let max = state
            .settings
            .orchestrate
            .child_models
            .iter_mut()
            .find(|child| child.model == "gpt-6-astra")
            .unwrap();
        max.fast = true;
    });

    let dispatch = |state: &mut AppState, cx: &mut HostCx, effort: &str, fast: Option<bool>| {
        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Dispatch {
                purpose: orchestrate_mcp::ThreadPurpose::Execution,
                parent_id: parent_id.clone(),
                provider: "codex".into(),
                model: Some("gpt-6-astra".into()),
                effort: Some(effort.into()),
                profile: None,
                access: None,
                title: "Child".into(),
                brief: "Inspect the workspace".into(),
                cwd: None,
                worktree: None,
                archive_on_complete: None,
                result_max_chars: None,
                fast,
            },
            reply,
            cx,
        );
        let id = response.try_recv().unwrap().unwrap()["thread_id"]
            .as_str()
            .unwrap()
            .to_string();
        state
            .find_meta(&id)
            .unwrap()
            .option_selections
            .iter()
            .any(|selection| selection.id == "serviceTier" && selection.value == "fast")
    };

    state.update(cx, |state, cx| {
        assert!(dispatch(state, cx, "medium", None), "model default: on");
        assert!(dispatch(state, cx, "medium", Some(true)), "override on");
        assert!(dispatch(state, cx, "max", None), "profile default: on");
        assert!(!dispatch(state, cx, "max", Some(false)), "override off");
    });
}

#[test]
fn orchestrate_dispatch_resolves_cwd_before_reply() {
    let cx = &mut TestAppContext::default();
    let root = std::env::temp_dir().join(format!("tcode-dispatch-cwd-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let test_store = TestStore::new("tcode-dispatch-cwd-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));
    let parent = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
    let parent_id = parent.id.clone();
    let missing = root.join("missing");
    let (reply, response) = smol::channel::bounded(1);

    state.update(cx, |state, cx| {
        state.sessions.push(parent);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Dispatch {
                purpose: orchestrate_mcp::ThreadPurpose::Execution,
                parent_id,
                provider: "codex".into(),
                model: Some("gpt-6-astra".into()),
                effort: None,
                profile: None,
                access: None,
                title: "Child".into(),
                brief: "Inspect the workspace".into(),
                cwd: Some(missing.to_string_lossy().into_owned()),
                worktree: None,
                archive_on_complete: None,
                result_max_chars: None,
                fast: None,
            },
            reply,
            cx,
        );
    });
    assert!(
        response.try_recv().is_err(),
        "cwd resolution must not reply from the GPUI update"
    );

    assert_eq!(
        recv_dispatch_reply(cx, &response).unwrap_err(),
        format!("invalid cwd: {}", missing.display())
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn orchestrate_worktree_dispatch_resolves_child_cwd_to_worktree() {
    let Some(root) = std::env::var_os("TCODE_TEST_WORKTREE_DISPATCH_ROOT") else {
        let data = TestStore::new("tcode-dispatch-worktree");
        // The override must be inherited at process creation: other tests and
        // their background workers can read it without participating in a lock.
        let output = tcode_services::process::command(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::tests::orchestrate_worktree_dispatch_resolves_child_cwd_to_worktree",
                "--nocapture",
            ])
            .env("TCODE_TEST_WORKTREE_DISPATCH_ROOT", data.root())
            .env(
                "TCODE_WORKTREES_DIR",
                data.root().join("tcode-owned-worktrees"),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let root = PathBuf::from(root);
    let isolated_worktrees = root.join("tcode-owned-worktrees");
    let cx = &mut TestAppContext::default();
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-b", "main"]).unwrap();
    run_git(&root, &["config", "commit.gpgsign", "false"]).unwrap();
    run_git(&root, &["config", "core.hooksPath", ".no-hooks"]).unwrap();
    run_git(&root, &["config", "core.autocrlf", "false"]).unwrap();
    run_git(&root, &["config", "user.name", "tcode"]).unwrap();
    run_git(&root, &["config", "user.email", "tcode@localhost"]).unwrap();
    std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
    run_git(&root, &["add", "tracked.txt"]).unwrap();
    run_git(&root, &["commit", "-m", "initial"]).unwrap();

    let test_store = TestStore::new("tcode-dispatch-worktree-data");
    let scripted = scripted_provider(ProviderKind::Codex);
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher_for_test(scripted.launcher);
        state
    });
    let parent = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
    let parent_id = parent.id.clone();
    let (reply, response) = smol::channel::bounded(1);

    state.update(cx, |state, cx| {
        state.sessions.push(parent);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Dispatch {
                purpose: orchestrate_mcp::ThreadPurpose::Execution,
                parent_id,
                provider: "codex".into(),
                model: Some("gpt-6-astra".into()),
                effort: None,
                profile: None,
                access: None,
                title: "Isolated child".into(),
                brief: "Inspect the workspace".into(),
                cwd: None,
                worktree: Some(true),
                archive_on_complete: None,
                result_max_chars: None,
                fast: None,
            },
            reply,
            cx,
        );
    });
    let response = recv_dispatch_reply(cx, &response).unwrap();
    let child_id = response["thread_id"].as_str().unwrap().to_string();
    let expected_branch = format!("tcode/{child_id}");
    let expected_path = isolated_worktrees.join(&child_id);
    assert_eq!(
        response["worktree_path"],
        expected_path.display().to_string(),
        "dispatch response: {response}"
    );
    assert_eq!(response["worktree_branch"], expected_branch);
    state.update(cx, |state, _| {
        let child = state.find_meta(&child_id).unwrap();
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
    let mux = tcode_remote::HostMux::new(host.to_host.clone(), host.from_host.clone());
    let connect = || {
        let connection = mux.attach();
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
            topic: Topic::SessionEvents {
                session_id: id.clone(),
            },
            after: None,
        })
        .unwrap();
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
        // An ack is a FIFO fence through both the host and mux.
        link.command_blocking(Command::ClearRelaunchMarker).unwrap();
        let events = link.events();
        let mut records = 0;
        while let Ok(event) = events.try_recv() {
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
fn session_history_snapshot_pages_and_absolute_tail_cursors() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-pages");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        let records: Vec<SessionEventRecord> = (0..2000)
            .map(|index| SessionEventRecord {
                ts: Some(index),
                event: AgentEvent::Warning {
                    message: format!("event {index}"),
                },
            })
            .collect();
        state
            .event_records
            .insert("large".into(), SessionLog::from_records(records.clone()));
        let subscription = tcode_protocol::Subscription {
            topic: Topic::SessionEvents {
                session_id: "large".into(),
            },
            after: None,
        };
        let snapshot = state.subscription_snapshot(&subscription).unwrap();
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
                truncated,
            } = state.session_history_page("large", before, 200).unwrap()
            else {
                panic!("page")
            };
            assert!(!truncated);
            assert_eq!(from + page.len() as u64, before);
            loaded.splice(0..0, page);
            before = from;
        }
        assert_eq!(loaded, records);
        for after in [0, 17, 1800, 1999, 2000] {
            let snapshot = state
                .subscription_snapshot(&tcode_protocol::Subscription {
                    after: Some(after),
                    ..subscription.clone()
                })
                .unwrap();
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
            });
            records.push(SessionEventRecord {
                ts: Some(turn * 1000 + 1),
                event: AgentEvent::TurnStarted {
                    turn_id: format!("turn-{turn}"),
                },
            });
            for delta in 0..300u64 {
                records.push(SessionEventRecord {
                    ts: Some(turn * 1000 + 2 + delta),
                    event: AgentEvent::Delta {
                        item_id: "pi-assistant-0:0".into(),
                        kind: agent::DeltaKind::AssistantText,
                        text: "word ".into(),
                    },
                });
            }
            records.push(SessionEventRecord {
                ts: Some(turn * 1000 + 400),
                event: AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{turn}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            });
        }
        assert_eq!(turn_starts, [0, 303, 606, 909, 1212]);
        state
            .event_records
            .insert("streamed".into(), SessionLog::from_records(records.clone()));

        let snapshot = state
            .subscription_snapshot(&tcode_protocol::Subscription {
                topic: Topic::SessionEvents {
                    session_id: "streamed".into(),
                },
                after: None,
            })
            .unwrap();
        let ServerEvent::SessionSnapshot { from, .. } = snapshot.event else {
            panic!("snapshot")
        };
        assert_eq!(
            from, 909,
            "the 400th-from-last record falls inside turn 3, which loads whole"
        );

        let QueryResponse::SessionHistoryPage {
            from: page_from,
            records: page,
            truncated,
        } = state.session_history_page("streamed", from, 200).unwrap()
        else {
            panic!("page")
        };
        assert_eq!(
            page_from, 606,
            "a 200-record page grows back to its turn start"
        );
        assert_eq!(page, records[606..909]);
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
        for (offset, event) in events.iter().enumerate() {
            store
                .append_event(id, turn * 1000 + offset as u64, event)
                .unwrap();
        }
    }
    store.read_events(id)
}

/// A client scrolling to the top of a long thread fetches one page per turn;
/// every page must come from the log parsed when the thread was opened.
#[test]
fn history_pages_of_an_opened_session_parse_the_log_once() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-single-parse");
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, store.root().clone(), None);
    meta.id = "paged".into();
    store.upsert_meta(&meta).unwrap();
    let records = persist_streamed_turns(&store, "paged", 5);
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let reads_before_open = store.event_reads();

    state.update(cx, |state, cx| state.select_session("paged", cx));
    let (from, mut loaded) = state.update(cx, |state, _| {
        let snapshot = state
            .subscription_snapshot(&tcode_protocol::Subscription {
                topic: Topic::SessionEvents {
                    session_id: "paged".into(),
                },
                after: None,
            })
            .unwrap();
        let ServerEvent::SessionSnapshot {
            from,
            records,
            total_turns,
            ..
        } = snapshot.event
        else {
            panic!("snapshot")
        };
        assert_eq!(total_turns, 5, "the absolute turn count comes from the log");
        (from, records)
    });
    cx.run_until(|state| {
        state
            .resident("paged")
            .is_some_and(|session| session.timeline.turns.len() == 5)
    });
    state.update(cx, |state, _| {
        assert_eq!(
            state.resident("paged").unwrap().timeline.turns.len(),
            5,
            "the timeline load reuses the cached fold"
        );
        let mut before = from;
        while before > 0 {
            let QueryResponse::SessionHistoryPage {
                records: page,
                from,
                ..
            } = state.session_history_page("paged", before, 200).unwrap()
            else {
                panic!("page")
            };
            assert_eq!(from + page.len() as u64, before);
            loaded.splice(0..0, page);
            before = from;
        }
        assert_eq!(loaded, records);
    });
    assert_eq!(
        store.event_reads() - reads_before_open,
        1,
        "opening, loading the timeline and paging parsed the log once"
    );
}

/// The cached log follows residency: it serves appends whose disk writes
/// are still queued, survives parking, and is dropped only after the session
/// leaves residency and the store writer has flushed those appends, so the
/// next cold open replays the whole conversation.
#[test]
fn session_log_follows_residency_and_flushes_before_release() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("history-log-residency");
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, store.root().clone(), None);
    meta.id = "resident".into();
    store.upsert_meta(&meta).unwrap();
    let persisted = persist_streamed_turns(&store, "resident", 2).len() as u64;
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let reads_before_open = store.event_reads();
    let (commands, _actor) = smol::channel::unbounded();

    state.update(cx, |state, cx| {
        state.select_session("resident", cx);
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
        );
        let snapshot = state
            .subscription_snapshot(&tcode_protocol::Subscription {
                topic: Topic::SessionEvents {
                    session_id: "resident".into(),
                },
                after: Some(persisted),
            })
            .unwrap();
        let ServerEvent::SessionSnapshot {
            records,
            total,
            total_turns,
            ..
        } = snapshot.event
        else {
            panic!("snapshot")
        };
        assert_eq!((total, total_turns), (persisted + 1, 3));
        assert!(
            matches!(&records[..], [record] if matches!(&record.event, AgentEvent::ItemCompleted(item) if item.id == "user-late"))
        );
        let QueryResponse::SessionHistoryPage { from, records, .. } = state
            .session_history_page("resident", persisted + 1, 1)
            .unwrap()
        else {
            panic!("page")
        };
        assert_eq!((from, records.len()), (persisted, 1));
        assert_eq!(
            state.store.event_reads() - reads_before_open,
            1,
            "the appended record is served from the log, not a re-parse"
        );

        // Parked with a live provider, the session stays resident and cached.
        state.selected_session_mut().unwrap().runtime = Runtime::Live(commands);
        state.park_active(cx);
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
    state.update(cx, |state, _| {
        assert!(!state.event_records.contains_key("resident"));
        assert_eq!(
            state.store.read_events("resident").len() as u64,
            persisted + 1
        );
    });
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
                ts: Some(index),
                event: AgentEvent::Warning {
                    message: "x".repeat(1024 * 1024),
                },
            })
            .collect();
        state
            .event_records
            .insert("large".into(), SessionLog::from_records(records.clone()));
        let subscription = tcode_protocol::Subscription {
            topic: Topic::SessionEvents {
                session_id: "large".into(),
            },
            after: None,
        };
        let mut snapshot = state.subscription_snapshot(&subscription).unwrap();
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
        let response = state.session_history_page("large", 10, u32::MAX).unwrap();
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
        } = response
        else {
            panic!("page")
        };
        assert!(truncated);
        assert_eq!(page, records[from as usize..]);
        let snapshot = state
            .subscription_snapshot(&tcode_protocol::Subscription {
                after: Some(0),
                ..subscription
            })
            .unwrap();
        let ServerEvent::SessionSnapshot {
            from,
            records: tail,
            truncated,
            ..
        } = snapshot.event
        else {
            panic!("tail")
        };
        assert_eq!(from, 0);
        assert!(truncated);
        assert_eq!(tail, records[..tail.len()]);
        let mut records = records;
        records[9].event = AgentEvent::Warning {
            message: "x".repeat(tcode_protocol::MAX_SESSION_HISTORY_BYTES),
        };
        state
            .event_records
            .insert("large".into(), SessionLog::from_records(records));
        assert_eq!(
            state.session_history_page("large", 10, 1).unwrap_err().code,
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
    let (session, commands) = fake_live_session(std::env::temp_dir());
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
fn settled_commands_persist_without_closing_the_selected_conversation() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-settled-lifecycle");
    let store = (*test_store).clone();
    for (id, parent) in [
        ("parent", None),
        ("child", Some("parent")),
        ("sibling", Some("parent")),
    ] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, store.root().clone(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_string);
        store.upsert_meta(&meta).unwrap();
    }
    let state = cx.new_entity(TestClientState::new(store.clone()));
    state.update(cx, |state, cx| state.select_session("parent", cx));
    cx.run_until_parked();
    state.dispatch_command(
        cx,
        1,
        Command::SettleSession {
            session_id: "parent".into(),
        },
    );
    cx.run_until_parked();
    state.read(|state| {
        assert_eq!(state.active_session_id(), Some("parent"));
        assert!(state.resident("parent").is_some());
        assert!(
            state
                .sessions
                .iter()
                .all(|meta| meta.settled_at.is_some() && meta.archived_at.is_none())
        );
    });
    let restarted = AppState::new(store.clone());
    assert!(
        restarted
            .sessions
            .iter()
            .all(|meta| meta.settled_at.is_some())
    );
    state.update(cx, |state, cx| state.select_session("child", cx));
    cx.run_until_parked();
    state.read(|state| assert!(state.find_meta("child").unwrap().settled_at.is_some()));
    state.dispatch_command(
        cx,
        2,
        Command::ArchiveSession {
            session_id: "parent".into(),
        },
    );
    state.dispatch_command(
        cx,
        3,
        Command::UnarchiveSession {
            session_id: "parent".into(),
        },
    );
    cx.run_until_parked();
    state.read(|state| {
        assert!(
            state
                .sessions
                .iter()
                .all(|meta| meta.settled_at.is_some() && meta.archived_at.is_none())
        )
    });
    state.dispatch_command(
        cx,
        4,
        Command::MakeSessionActive {
            session_id: "parent".into(),
        },
    );
    cx.run_until_parked();
    assert!(
        store
            .load_index()
            .iter()
            .all(|meta| meta.settled_at.is_none())
    );
}

#[test]
fn settling_rejects_busy_descendants_and_accepted_input_reactivates_ancestors() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-settled-input");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let (commands, _receiver) = smol::channel::unbounded();
    state.update(cx, |state, _| {
        let mut child = live_session(ProviderKind::Codex, commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        child.meta.settled_at = Some(1);
        let mut parent = SessionMeta::new(ProviderKind::Codex, test_store.root().clone(), None);
        parent.id = "parent".into();
        parent.settled_at = Some(1);
        let mut sibling = parent.clone();
        sibling.id = "sibling".into();
        sibling.parent_session_id = Some("parent".into());
        state.sessions.extend([parent, sibling, child.meta.clone()]);
        state.install_selected(child);
    });
    state.dispatch_command(
        cx,
        1,
        Command::ScheduleTurn {
            session_id: "child".into(),
            text: "later".into(),
            attachment_paths: Vec::new(),
            fire_at_unix_secs: now_secs() + 3600,
        },
    );
    state.read(|state| {
        assert!(state.find_meta("child").unwrap().settled_at.is_none());
        assert!(state.find_meta("parent").unwrap().settled_at.is_none());
        assert!(state.find_meta("sibling").unwrap().settled_at.is_some());
    });
    // Each state would hide reachable child work if the parent were allowed to settle.
    for busy in ["queued", "turn", "background", "input", "approval"] {
        state.update(cx, |state, _| {
            let child = state.resident_mut("child").unwrap();
            child.queue.clear();
            child.turn_in_flight = busy == "turn";
            child.background_task_count = usize::from(busy == "background");
            child.timeline.pending_user_input =
                (busy == "input").then(|| ("input".into(), Vec::new()));
            child.timeline.pending_approvals.clear();
            if busy == "queued" {
                child.push_queued("queued".into(), Vec::new());
            }
            if busy == "approval" {
                child
                    .timeline
                    .pending_approvals
                    .push(agent::ApprovalRequest {
                        id: "approval".into(),
                        turn_id: None,
                        kind: agent::ApprovalKind::ExecCommand {
                            command: "pwd".into(),
                            cwd: None,
                            reason: None,
                        },
                        options: Vec::new(),
                    });
            }
            assert_eq!(
                state
                    .validate_command_target(&Command::SettleSession {
                        session_id: "parent".into()
                    })
                    .unwrap_err()
                    .code,
                "thread_busy"
            );
        });
        state.dispatch_command(
            cx,
            2,
            Command::SettleSession {
                session_id: "parent".into(),
            },
        );
        state.read(|state| assert!(state.find_meta("parent").unwrap().settled_at.is_none()));
    }
    state.update(cx, |state, _| {
        let child = state.resident_mut("child").unwrap();
        child.timeline.pending_approvals.clear();
        child.timeline.pending_user_input = None;
        child.turn_in_flight = false;
        child.background_task_count = 0;
    });
    state.dispatch_command(
        cx,
        3,
        Command::SettleSession {
            session_id: "parent".into(),
        },
    );
    state.dispatch_command(
        cx,
        4,
        Command::SendTurn {
            session_id: "child".into(),
            text: "continue".into(),
            attachment_paths: Vec::new(),
        },
    );
    state.read(|state| {
        assert!(state.find_meta("parent").unwrap().settled_at.is_none());
        assert!(state.find_meta("child").unwrap().settled_at.is_none());
        assert!(state.find_meta("sibling").unwrap().settled_at.is_some());
    });
    cx.run_until_parked();
}

#[test]
fn interrupt_reports_stopping_until_the_turn_completes() {
    let store = TestStore::new("interrupt-stopping");
    let mut state = AppState::new((*store).clone());
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
    assert!(!status.turn_running);
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
    fs::copy(&source, store.root().join("bench.jsonl")).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let started = Instant::now();
    state.update(cx, |state, cx| state.select_session("bench", cx));
    let opened = started.elapsed();
    let (snapshot_elapsed, pages) = state.update(cx, |state, _| {
        let started = Instant::now();
        let snapshot = state
            .subscription_snapshot(&tcode_protocol::Subscription {
                topic: Topic::SessionEvents {
                    session_id: "bench".into(),
                },
                after: None,
            })
            .unwrap();
        let snapshot_elapsed = started.elapsed();
        let ServerEvent::SessionSnapshot { from, total, .. } = snapshot.event else {
            panic!("snapshot")
        };
        eprintln!("snapshot: {snapshot_elapsed:?} (from {from} of {total} records)");
        let mut before = from;
        let mut pages = Vec::new();
        while before > 0 && pages.len() < 16 {
            let started = Instant::now();
            let QueryResponse::SessionHistoryPage { from, records, .. } = state
                .session_history_page(
                    "bench",
                    before,
                    tcode_protocol::SESSION_HISTORY_RECORDS as u32,
                )
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
