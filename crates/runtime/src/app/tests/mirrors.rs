use super::*;

/// A client learns what a thread it does not hold is doing from the index
/// alone, and only when that changes: a stream sends nothing, the next item
/// sends one update, an item saying the same again sends none, and the
/// turn's end clears it.
#[test]
fn only_a_new_item_publishes_a_threads_latest_activity() {
    use tcode_core::session::LatestActivity;
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-latest-activity");
    stored_thread(
        &test_store,
        "child",
        ProviderKind::Codex,
        &test_store.root().join("child"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let provider = first_turn(&state, cx, &providers, "child", 0);
    cx.run_until_seen(|view| {
        view.activity("child")
            .is_some_and(|activity| activity.turn_running)
    });
    cx.drain_outgoing();

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
                input: Vec::new(),
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
    provider.play(std::iter::once(item(
        "thought",
        ItemContent::Reasoning {
            text: String::new(),
        },
    )));
    provider.play((0..50).map(|_| delta("thought", agent::DeltaKind::ReasoningText, "more")));
    provider.play([
        command("first"),
        delta("first", agent::DeltaKind::CommandOutput, "ok\n"),
        command("second"),
        turn_completed("turn", TurnStatus::Completed),
    ]);
    cx.run_until_seen(|view| has_record(view, "child", is_turn_end));

    let published = published_activity(cx, "child");
    let (during, after) = published.split_at(2);
    assert_eq!(
        during,
        [
            Some(LatestActivity::Thinking),
            Some(LatestActivity::Command {
                text: "cargo test".into()
            })
        ]
    );
    assert!(
        !after.is_empty() && after.iter().all(Option::is_none),
        "{after:?}"
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
    stored_thread(
        &test_store,
        "lead",
        ProviderKind::ClaudeCode,
        &test_store.root().join("lead"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let provider = first_turn(&state, cx, &providers, "lead", 0);

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
    provider.play([AgentEvent::ItemStarted(spawn(ItemStatus::InProgress))]);
    cx.run_until_seen(|view| view.mirror("spawn").is_some());
    let mirror = cx.seen().mirror("spawn").unwrap().id.clone();
    provider.play([AgentEvent::ItemStarted(ThreadItem {
        id: "child".into(),
        parent_item_id: Some("spawn".into()),
        content: ItemContent::CommandExecution {
            command: "ls".into(),
            input: Vec::new(),
            output: String::new(),
            exit_code: None,
            status: ItemStatus::InProgress,
        },
    })]);
    let ls = Some(LatestActivity::Command { text: "ls".into() });
    cx.run_until_seen(|view| view.activity(&mirror).unwrap().latest_activity == ls);
    assert_eq!(
        cx.seen().activity("lead").unwrap().latest_activity,
        Some(LatestActivity::Subagent {
            description: "Audit".into()
        })
    );

    provider.play([AgentEvent::ItemCompleted(spawn(ItemStatus::Completed))]);
    cx.run_until_seen(|view| {
        has_record(
            view,
            "lead",
            |event| matches!(event, AgentEvent::ItemCompleted(item) if item.id == "spawn"),
        )
    });
    assert_eq!(cx.seen().activity(&mirror).unwrap().latest_activity, None);
}

/// A request a harness's own subagent raises is answered on its lead, so the
/// lead's index entry says which subagent waits and for what, as its spawn
/// names it wherever it was spawned, until the request is answered.
#[test]
fn a_native_subagents_request_is_named_on_its_lead_until_answered() {
    use tcode_protocol::{AskingKind, SubagentAsking};
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-subagent-asking");
    stored_thread(
        &test_store,
        "lead",
        ProviderKind::ClaudeCode,
        &test_store.root().join("lead"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let provider = first_turn(&state, cx, &providers, "lead", 0);

    provider.play([
        AgentEvent::ItemStarted(subagent_item(
            "spawn",
            None,
            "Explore",
            "Audit the routing",
            ItemStatus::InProgress,
        )),
        AgentEvent::ItemStarted(subagent_item(
            "nested",
            Some("spawn"),
            "general-purpose",
            "Read the router",
            ItemStatus::InProgress,
        )),
        AgentEvent::ApprovalRequested(agent::ApprovalRequest {
            id: "approval".into(),
            turn_id: None,
            parent_item_id: Some("nested".into()),
            kind: agent::ApprovalKind::ExecCommand {
                command: "cp a b".into(),
                cwd: None,
                description: None,
            },
            reason: None,
            blocked_path: None,
            options: vec![agent::ApprovalOption {
                id: "allow".into(),
                label: "Allow".into(),
                kind: agent::ApprovalOptionKind::AllowOnce,
            }],
        }),
        AgentEvent::UserInputRequested {
            request_id: "question".into(),
            parent_item_id: Some("spawn".into()),
            questions: Vec::new(),
            delivery: agent::UserInputDelivery::Blocking,
        },
    ]);
    let asking = |cx: &TestAppContext| cx.seen().activity("lead").unwrap().asking.clone();
    cx.run_until_seen(|view| view.activity("lead").unwrap().asking.len() == 2);
    assert_eq!(
        asking(cx),
        [
            SubagentAsking {
                item_id: "nested".into(),
                agent_type: "general-purpose".into(),
                description: "Read the router".into(),
                kind: AskingKind::Approval,
            },
            SubagentAsking {
                item_id: "spawn".into(),
                agent_type: "Explore".into(),
                description: "Audit the routing".into(),
                kind: AskingKind::Question,
            },
        ]
    );

    state
        .command(
            cx,
            Command::RespondApproval {
                session_id: "lead".into(),
                request_id: "approval".into(),
                decision: ApprovalDecision::Option("allow".into()),
            },
        )
        .unwrap();
    cx.run_until_seen(|view| view.activity("lead").unwrap().asking.len() == 1);
    assert_eq!(asking(cx)[0].kind, AskingKind::Question);

    state
        .command(
            cx,
            Command::RespondUserInput {
                session_id: "lead".into(),
                request_id: "question".into(),
                answers: serde_json::Map::new(),
            },
        )
        .unwrap();
    provider.play([AgentEvent::UserInputResolved {
        request_id: "question".into(),
        answers: serde_json::Map::new(),
    }]);
    cx.run_until_seen(|view| view.activity("lead").unwrap().asking.is_empty());
}

/// A log whose turn a dead host left open still folds to a running timeline,
/// but nothing is running it, so the thread reports no activity.
#[test]
fn a_turn_left_open_in_the_log_reports_no_activity() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-stale-activity");
    stored_thread(
        &test_store,
        "stale",
        ProviderKind::Codex,
        &test_store.root().join("stale"),
        |_| {},
    );
    for (ts, event) in [
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
    ]
    .iter()
    .enumerate()
    {
        test_store.append_event("stale", ts as u64, event).unwrap();
    }
    let (state, _providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    state.open(cx, "stale");
    assert_eq!(cx.seen().activity("stale").unwrap().latest_activity, None);
}

/// A harness's own subagent becomes a read-only thread under the lead,
/// titled by its description's first line and running the lead's model and
/// options until it names its own; its transcript goes there and not to the
/// lead, and its one turn is the spawn's. A host started again finds the
/// mirror as it last was.
#[test]
fn provider_native_subagent_events_create_and_feed_read_only_mirror_session() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-native-subagent-mirror-test");
    let cwd = test_store.root().join("parent");
    stored_thread(&test_store, "parent", ProviderKind::Codex, &cwd, |meta| {
        meta.model = Some("gpt-test".into());
        meta.option_selections = vec![OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("high"),
        }];
    });
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let provider = first_turn(&state, cx, &providers, "parent", 0);
    let spawn = |description: &str, status, summary: Option<&str>, model: Option<&str>| {
        let item = ThreadItem {
            id: "spawn-1".into(),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: "explorer".into(),
                description: description.into(),
                status,
                summary: summary.map(Into::into),
                model: model.map(Into::into),
                effort: model.map(|_| "low".into()),
            },
        };
        match (status, model) {
            (ItemStatus::InProgress, None) => AgentEvent::ItemStarted(item),
            (ItemStatus::InProgress, Some(_)) => AgentEvent::ItemUpdated(item),
            _ => AgentEvent::ItemCompleted(item),
        }
    };
    let is_spawn = |event: &AgentEvent| {
        matches!(
            event,
            AgentEvent::ItemUpdated(item) | AgentEvent::ItemCompleted(item) if item.id == "spawn-1"
        )
    };

    provider.play([spawn(
        "Inspect event routing\nand report",
        ItemStatus::InProgress,
        None,
        None,
    )]);
    cx.run_until_seen(|view| view.mirror("spawn-1").is_some());
    let mirror = cx.seen().mirror("spawn-1").unwrap().clone();
    assert_eq!(mirror.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(mirror.title, "explorer: Inspect event routing");
    assert_eq!(mirror.option_selections[0].value, serde_json::json!("high"));
    assert_eq!(mirror.model.as_deref(), Some("gpt-test"));
    state.open(cx, &mirror.id);

    provider.play([spawn(
        "Inspect event routing",
        ItemStatus::InProgress,
        None,
        Some("child-model"),
    )]);
    cx.run_until_seen(|view| has_record(view, "parent", is_spawn));
    let status = cx.seen().status(&mirror.id);
    assert_eq!(status.requested_model.as_deref(), Some("child-model"));
    assert_eq!(
        status.provider_option_selections[0].value,
        serde_json::json!("low")
    );
    assert!(status.activity.turn_running);
    assert_eq!(
        cx.seen().status("parent").requested_model.as_deref(),
        Some("gpt-test")
    );

    provider.play([AgentEvent::ItemCompleted(ThreadItem {
        id: "child-answer".into(),
        parent_item_id: Some("spawn-1".into()),
        content: ItemContent::AssistantMessage {
            text: "routed transcript".into(),
            summarized: false,
        },
    })]);
    provider.play([spawn(
        "Inspect event routing\nand report",
        ItemStatus::Completed,
        Some("routing verified"),
        None,
    )]);
    cx.run_until_seen(|view| {
        has_record(view, "parent", |event| {
            is_spawn(event) && matches!(event, AgentEvent::ItemCompleted(_))
        })
    });
    let answer = |timeline: &Timeline| {
        timeline.entries.iter().any(|entry| {
            matches!(
                &entry.content,
                EntryContent::Item(ItemContent::AssistantMessage { text, .. })
                    if entry.id == "child-answer" && text == "routed transcript"
            )
        })
    };
    assert!(answer(&cx.seen().timeline(&mirror.id)));
    assert!(!cx.seen().status(&mirror.id).activity.turn_running);
    let parent = cx.seen().timeline("parent");
    assert!(!answer(&parent));
    assert!(parent.entries.iter().any(|entry| matches!(
        &entry.content,
        EntryContent::Item(ItemContent::Subagent {
            status: ItemStatus::Completed,
            summary: Some(summary),
            ..
        }) if summary == "routing verified"
    )));

    // The completion named no model or effort; the mirror keeps the ones it
    // last named, and its transcript is its own.
    let cx = &mut TestAppContext::default();
    let (state, _providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    state.open(cx, &mirror.id);
    state.open(cx, "parent");
    let reopened = cx.seen().session(&mirror.id).unwrap();
    assert_eq!(reopened.model.as_deref(), Some("child-model"));
    assert_eq!(
        reopened.option_selections[0].value,
        serde_json::json!("low")
    );
    assert!(has_record(cx.seen(), &mirror.id, |event| matches!(
        event,
        AgentEvent::ItemCompleted(ThreadItem { id, parent_item_id: None, .. })
            if id == "child-answer"
    )));
    assert!(
        cx.seen()
            .records("parent")
            .iter()
            .all(|stored| match &stored.event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item) => item.parent_item_id.is_none(),
                _ => true,
            })
    );
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
    stored_thread(
        &test_store,
        "parent",
        ProviderKind::OpenCode,
        &test_store.root().join("parent"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
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
    let provider = first_turn(&state, cx, &providers, "parent", 0);
    provider.play([
        AgentEvent::ItemStarted(subagent(ItemStatus::InProgress, None)),
        AgentEvent::ItemStarted(text(child, Some(spawn), "")),
        delta(child, "KIWI"),
        AgentEvent::ItemCompleted(text(child, Some(spawn), "KIWI")),
        AgentEvent::ItemCompleted(subagent(ItemStatus::Completed, Some("KIWI"))),
        AgentEvent::ItemStarted(text(reply, None, "")),
        delta(reply, "The subagent said: KIWI"),
        AgentEvent::ItemCompleted(text(reply, None, "The subagent said: KIWI")),
        turn_completed("turn", TurnStatus::Completed),
    ]);
    cx.run_until_seen(|view| has_record(view, "parent", is_turn_end));
    let mirror = cx.seen().mirror(spawn).unwrap().id.clone();
    let ids = |timeline: &Timeline| -> Vec<String> {
        timeline
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.content,
                    EntryContent::Item(ItemContent::AssistantMessage { .. })
                        | EntryContent::Item(ItemContent::Subagent { .. })
                )
            })
            .map(|entry| entry.id.clone())
            .collect()
    };
    assert_eq!(
        ids(&cx.seen().timeline("parent")),
        [spawn, reply],
        "the parent thread as its client folds it"
    );
    state.open(cx, &mirror);
    assert_eq!(
        ids(&cx.seen().timeline(&mirror)),
        [child],
        "the mirror holds the child's reply"
    );
    let cx = &mut TestAppContext::default();
    let (state, _providers) = played_host(cx, &test_store);
    state.open(cx, "parent");
    assert_eq!(
        ids(&cx.seen().timeline("parent")),
        [spawn, reply],
        "the parent thread reopened from its log"
    );
}

/// An item a provider sends after closing never reaches the host, so a late
/// one follows only an ending the provider goes on from.
#[test]
fn native_mirror_keeps_one_turn_across_residency_late_items_and_parent_completion() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-native-mirror-turn-lifecycle");
    let mut cases = Vec::new();
    for evict in [false, true] {
        for late in [false, true] {
            for end in [
                MirrorEnd::SubagentCompleted,
                MirrorEnd::ParentTurnThenSubagentCompleted,
                MirrorEnd::SessionClosed,
            ] {
                if late && end == MirrorEnd::SessionClosed {
                    continue;
                }
                let parent = native_mirror_parent(evict, late, end);
                stored_thread(
                    &test_store,
                    &parent,
                    ProviderKind::Codex,
                    &test_store.root().join(&parent),
                    |_| {},
                );
                cases.push((evict, late, end));
            }
        }
    }
    for filler in NATIVE_MIRROR_FILLERS {
        stored_thread(
            &test_store,
            filler,
            ProviderKind::Codex,
            &test_store.root().join(filler),
            |_| {},
        );
        test_store
            .append_event(
                filler,
                1,
                &AgentEvent::ItemCompleted(ThreadItem {
                    id: format!("{filler}-message"),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: "Hello".into(),
                        attachments: Vec::new(),
                    },
                }),
            )
            .unwrap();
    }
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    for (launch, (evict, late, end)) in cases.into_iter().enumerate() {
        assert_native_mirror_turn_lifecycle(cx, &state, &providers, launch, evict, late, end);
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
    stored_thread(
        &test_store,
        "parent",
        ProviderKind::ClaudeCode,
        &test_store.root().join("parent"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let provider = first_turn(&state, cx, &providers, "parent", 0);
    provider.play([
        AgentEvent::ItemStarted(subagent_item(
            "toolu_child",
            None,
            "general-purpose",
            "Research hosting",
            ItemStatus::InProgress,
        )),
        AgentEvent::ItemStarted(subagent_item(
            "toolu_grandchild",
            Some("toolu_child"),
            "Explore",
            "Research Workers limits\nwith sources",
            ItemStatus::InProgress,
        )),
    ]);
    cx.run_until_seen(|view| view.mirror("toolu_grandchild").is_some());
    let child_id = cx.seen().mirror("toolu_child").unwrap().id.clone();
    let grandchild = cx
        .seen()
        .mirror("toolu_grandchild")
        .cloned()
        .expect("the spawn alone opens the grandchild's mirror");
    assert_eq!(
        grandchild.parent_session_id.as_deref(),
        Some(child_id.as_str())
    );
    assert_eq!(grandchild.title, "Explore: Research Workers limits");
    state.open(cx, &child_id);
    assert!(cx.seen().timeline(&child_id).entries.iter().any(|entry| {
        entry.id == "toolu_grandchild"
            && matches!(
                entry.content,
                EntryContent::Item(ItemContent::Subagent { .. })
            )
    }));

    // Launched in the background, it is the child's background work.
    provider.play([AgentEvent::ItemBackgrounded {
        item_id: "toolu_grandchild".into(),
        parent_item_id: Some("toolu_child".into()),
    }]);
    cx.run_until_seen(|view| {
        has_record(view, &child_id, |event| {
            matches!(event, AgentEvent::ItemBackgrounded { .. })
        })
    });
    assert!(matches!(
        cx.seen().status(&child_id).background_tasks.as_slice(),
        [listed] if listed.task.item_id.as_deref() == Some("toolu_grandchild")
            && listed.task.kind == agent::BackgroundKind::Agent
    ));
    assert!(cx.seen().status("parent").background_tasks.is_empty());

    state.open(cx, &grandchild.id);
    provider.play([AgentEvent::ItemCompleted(ThreadItem {
        id: "toolu_grandchild:user-1".into(),
        parent_item_id: Some("toolu_grandchild".into()),
        content: ItemContent::UserMessage {
            text: "Fetch the limits page.".into(),
            attachments: Vec::new(),
        },
    })]);
    cx.run_until_seen(|view| {
        view.timeline(&grandchild.id)
            .entries
            .iter()
            .any(|entry| entry.id == "toolu_grandchild:user-1")
    });
    assert_eq!(
        cx.seen()
            .sessions()
            .filter(|meta| meta.native_subagent.as_deref() == Some("toolu_grandchild"))
            .count(),
        1,
        "the grandchild's transcript goes to the mirror its spawn opened"
    );
    assert_eq!(
        cx.seen()
            .sessions()
            .filter(|meta| meta.parent_session_id.as_deref() == Some("parent"))
            .count(),
        1,
        "the root session owns only the child it spawned"
    );

    // The grandchild's terminal snapshot travels as child content too.
    provider.play([
        AgentEvent::ItemUpdated(subagent_item(
            "toolu_grandchild",
            Some("toolu_child"),
            "Explore",
            "Research Workers limits",
            ItemStatus::Completed,
        )),
        AgentEvent::ItemCompleted(subagent_item(
            "toolu_child",
            None,
            "general-purpose",
            "Research hosting",
            ItemStatus::Completed,
        )),
    ]);
    cx.run_until_seen(|view| {
        has_record(
            view,
            "parent",
            |event| matches!(event, AgentEvent::ItemCompleted(item) if item.id == "toolu_child"),
        )
    });
    let parent_items = cx
        .seen()
        .timeline("parent")
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.content,
                EntryContent::Item(ref item) if !matches!(item, ItemContent::UserMessage { .. })
            )
        })
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(parent_items, ["toolu_child"]);

    // A second nested spawn is still running when the root process closes:
    // it belongs to that process, so it ends with it.
    provider.play([
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
        AgentEvent::SessionClosed { reason: None },
    ]);
    cx.run_until_seen(|view| {
        has_record(view, "parent", |event| {
            matches!(event, AgentEvent::SessionClosed { .. })
        })
    });
    let second = cx
        .seen()
        .mirror("toolu_grandchild_2")
        .cloned()
        .expect("second grandchild mirror");
    assert_eq!(second.parent_session_id.as_deref(), Some(child_id.as_str()));
    state.open(cx, &second.id);
    let records = cx.seen().records(&grandchild.id);
    assert!(matches!(
        &records.first().unwrap().event,
        AgentEvent::TurnStarted { turn_id } if turn_id == "toolu_grandchild"
    ));
    assert!(matches!(
        &records.last().unwrap().event,
        AgentEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }
    ));
    assert!(matches!(
        &cx.seen().records(&second.id).last().unwrap().event,
        AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Interrupted, .. }
            if turn_id == "toolu_grandchild_2"
    ));
    assert!(
        cx.seen()
            .records(&child_id)
            .iter()
            .all(|stored| match &stored.event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item) => item.parent_item_id.is_none(),
                _ => true,
            })
    );
}

/// A child item whose spawn this session never announced still gets a mirror,
/// and the parent process closing ends it like every other running mirror.
#[test]
fn unknown_parent_mirror_closes_when_the_parent_session_closes() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-unknown-parent-native-mirror");
    stored_thread(
        &test_store,
        "parent",
        ProviderKind::ClaudeCode,
        &test_store.root().join("parent"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let provider = first_turn(&state, cx, &providers, "parent", 0);
    provider.play([AgentEvent::ItemCompleted(ThreadItem {
        id: "toolu_orphan:msg:0".into(),
        parent_item_id: Some("toolu_orphan".into()),
        content: ItemContent::AssistantMessage {
            text: "working".into(),
            summarized: false,
        },
    })]);
    cx.run_until_seen(|view| view.mirror("toolu_orphan").is_some());
    let mirror = cx.seen().mirror("toolu_orphan").unwrap().clone();
    assert_eq!(mirror.title, "subagent");
    provider.play([AgentEvent::SessionClosed { reason: None }]);
    cx.run_until_seen(|view| {
        has_record(view, "parent", |event| {
            matches!(event, AgentEvent::SessionClosed { .. })
        })
    });
    state.open(cx, &mirror.id);
    assert!(matches!(
        &cx.seen().records(&mirror.id).last().unwrap().event,
        AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Interrupted, .. }
            if turn_id == "toolu_orphan"
    ));
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

    let (state, _providers) = played_host(cx, &test_store);
    state.open(cx, "mirror");
    cx.run_until_seen(|view| has_record(view, "mirror", is_turn_end));
    let records = cx.seen().records("mirror");
    assert_eq!(records.len(), 3);
    assert!(matches!(
        &records[2].event,
        AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Interrupted, .. }
            if turn_id == "toolu_zombie"
    ));
    let timeline = cx.seen().timeline("mirror");
    assert!(!timeline.turn_running);
    assert_eq!(timeline.turns[0].status, Some(TurnStatus::Interrupted));
    assert!(!cx.seen().status("mirror").activity.turn_running);
}
