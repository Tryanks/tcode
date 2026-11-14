use super::test_support::*;
use super::*;
use super::{active_session::*, events::*, orchestrate::*, providers::*};

use tcode_core::notice::CHILD_REPORT_REMINDER;
use tcode_core::project::ChildExpectation;
use tcode_core::settings::{SettingsPatch, ThemeMode};
use tcode_protocol::{BackgroundTaskStatus, Command, CommandResponse, HostMessage, Subscription};

/// What crosses the wire for `thread`'s index entry from now on, in order.
fn published_activity(
    cx: &mut TestAppContext,
    thread: &str,
) -> Vec<Option<tcode_core::session::LatestActivity>> {
    cx.drain_outgoing()
        .into_iter()
        .filter_map(|message| match message {
            HostMessage::Event(envelope) => match envelope.event {
                ServerEvent::IndexActivitiesChanged { mut upserts, .. } => upserts
                    .remove(thread)
                    .map(|activity| activity.latest_activity),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Whether the client holds a record of `id`'s log that `matches`.
fn has_record(view: &ClientView, id: &str, matches: impl Fn(&AgentEvent) -> bool) -> bool {
    view.records(id).iter().any(|record| matches(&record.event))
}

fn turn_completed(turn_id: &str, status: TurnStatus) -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: turn_id.into(),
        status,
        usage: None,
    }
}

fn is_turn_end(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::TurnCompleted { .. })
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

/// A lead's provider runs a subagent of its own: its mirror holds one turn,
/// from the spawn to its end, however the mirror leaves and comes back into
/// the host's memory, whatever arrives after the end, and whether the lead's
/// own turn ends first.
fn assert_native_mirror_turn_lifecycle(
    cx: &mut TestAppContext,
    state: &TestEntity,
    providers: &Providers,
    launch: usize,
    evict: bool,
    late: bool,
    end: MirrorEnd,
) {
    let parent = native_mirror_parent(evict, late, end);
    let parent = parent.as_str();
    let provider = first_turn(state, cx, providers, parent, launch);
    provider.play([native_mirror_parent_item(ItemStatus::InProgress)]);
    let mirror_of = |view: &ClientView| {
        view.sessions()
            .find(|meta| {
                meta.native_subagent.is_some() && meta.parent_session_id.as_deref() == Some(parent)
            })
            .map(|meta| meta.id.clone())
    };
    cx.run_until_seen(|view| mirror_of(view).is_some());
    let mirror_id = mirror_of(cx.seen()).unwrap();
    for i in 0..3 {
        if evict && i == 1 {
            // The user looked at the subagent's thread and went on to
            // others; the host lets it go from memory once more recent ones
            // are left too.
            state.open(cx, &mirror_id);
            cx.run_until_seen(|view| view.timeline(&mirror_id).entries.len() == 1);
            cx.clock.advance(Duration::from_secs(1));
            state.leave(cx, &mirror_id);
            for filler in NATIVE_MIRROR_FILLERS {
                cx.clock.advance(Duration::from_secs(1));
                state.open(cx, filler);
                state.leave(cx, filler);
            }
        }
        provider.play([native_mirror_child_item(i)]);
    }
    match end {
        MirrorEnd::SubagentCompleted => {
            provider.play([native_mirror_parent_item(ItemStatus::Completed)]);
        }
        MirrorEnd::ParentTurnThenSubagentCompleted => {
            provider.play([turn_completed("turn", TurnStatus::Completed)]);
            cx.run_until_seen(|view| has_record(view, parent, is_turn_end));
            if !evict {
                assert!(cx.seen().activity(&mirror_id).unwrap().turn_running);
            }
            provider.play([
                native_mirror_child_item(3),
                native_mirror_parent_item(ItemStatus::Completed),
            ]);
        }
        MirrorEnd::SessionClosed => {
            provider.play([AgentEvent::SessionClosed { reason: None }]);
        }
    }
    if late {
        provider.play([native_mirror_child_item(4)]);
    }
    if end == MirrorEnd::SessionClosed {
        cx.run_until_seen(|view| {
            has_record(view, parent, |event| {
                matches!(event, AgentEvent::SessionClosed { .. })
            })
        });
    } else {
        provider.fence(cx, parent);
    }

    let expected_items =
        3 + usize::from(end == MirrorEnd::ParentTurnThenSubagentCompleted) + usize::from(late);
    let expected_status = if end == MirrorEnd::SessionClosed {
        TurnStatus::Interrupted
    } else {
        TurnStatus::Completed
    };
    state.open(cx, &mirror_id);
    let (mut open, mut boundaries, mut items) = (false, 0, 0);
    for stored in cx.seen().records(&mirror_id) {
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
            AgentEvent::ItemCompleted(_) => items += 1,
            _ => {}
        }
    }
    assert!(!open, "persisted mirror boundaries must balance");
    assert_eq!(boundaries, 2, "exactly one turn, never a zero-length one");
    assert_eq!(items, expected_items);
    let timeline = cx.seen().timeline(&mirror_id);
    assert!(!timeline.turn_running);
    assert_eq!(timeline.turns.len(), 1);
    let turn = &timeline.turns[0];
    assert!(!turn.running && turn.start_ts.is_some() && turn.end_ts.is_some());
    assert_eq!(turn.status, Some(expected_status));
    assert!(!cx.seen().status(&mirror_id).activity.working);
    state.leave(cx, &mirror_id);
}

/// Threads only the user visits, enough of them that leaving them all lets
/// a thread left before them go from the host's memory.
const NATIVE_MIRROR_FILLERS: [&str; MAX_IDLE_REPLAY_RESIDENTS] =
    ["visited-1", "visited-2", "visited-3", "visited-4"];

fn native_mirror_parent(evict: bool, late: bool, end: MirrorEnd) -> String {
    format!("parent-{evict}-{late}-{end:?}")
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

/// A secrets.json this build cannot read at start: the host runs, clients
/// are told the saved secrets could not be read rather than shown none,
/// saving a secret says why it is refused, and the file is left as it is.
#[test]
fn unreadable_secrets_are_reported_and_never_written_over() {
    let test_store = TestStore::new("tcode-secrets-unreadable");
    std::fs::create_dir_all(test_store.root()).unwrap();
    let torn = r#"{"version":1,"profiles":{"codex":"#;
    std::fs::write(test_store.root().join("secrets.json"), torn).unwrap();
    let cx = &mut TestAppContext::default();
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
    let unreadable = cx
        .drain_outgoing()
        .into_iter()
        .find_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                event: ServerEvent::ProvidersReplaced(status),
                ..
            }) => Some(status.secrets_unreadable),
            _ => None,
        });
    assert!(
        unreadable
            .flatten()
            .is_some_and(|error| error.contains("secrets.json")),
        "the providers status says the secrets could not be read"
    );
    state.dispatch_command(
        cx,
        43,
        Command::SetProfileSecret {
            profile_id: "codex".into(),
            name: "OPENAI_API_KEY".into(),
            value: Some("typed-again".into()),
        },
    );
    cx.run_until_parked();
    let outgoing = cx.drain_outgoing();
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            event: ServerEvent::Runtime(RuntimeEvent::Error(RuntimeError::FileUnreadable { error })),
            ..
        }) if error.contains("secrets.json")
    )));
    assert!(!outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            event: ServerEvent::ProvidersReplaced(status),
            ..
        }) if status.secret_names.get("codex").is_some_and(|names| !names.is_empty())
    )));
    state.update(cx, |state, _| state.close_store().unwrap());
    assert_eq!(
        std::fs::read_to_string(test_store.root().join("secrets.json")).unwrap(),
        torn
    );
}

/// A finished turn refreshes the account usage of the profile it ran on and
/// of no other, and asks the thread's own running harness for it instead of
/// starting another. Claude Code reports `max` for the Ultra plan too, so the
/// plan is the one its account file names.
#[test]
fn a_turn_refreshes_only_its_profiles_usage_from_its_running_harness() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-turn-usage");
    let account = test_store.root().join("account");
    std::fs::create_dir_all(&account).unwrap();
    std::fs::write(
        account.join(".claude.json"),
        r#"{"oauthAccount":{"organizationType":"claude_max","planDisplayName":"Ultra"}}"#,
    )
    .unwrap();
    let mut settings = Settings::default();
    settings.provider_mut(ProviderKind::ClaudeCode).home_path = Some(account);
    SettingsStore::new(test_store.root().clone())
        .save(&settings)
        .unwrap();
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::ClaudeCode,
        &test_store.root().join("project"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Providers);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    provider.play([turn_completed("turn", TurnStatus::Completed)]);
    let reply = loop {
        if let SessionCommand::ReadAccountUsage { reply } = cx.run_until_recv(&provider.commands) {
            break reply;
        }
    };
    cx.run_until_parked();
    assert_eq!(
        cx.seen().providers().usage_checking,
        HashSet::from(["claude".to_string()])
    );
    reply
        .try_send(Ok(serde_json::json!({
            "subscription_type": "max",
            "rate_limits_available": true,
            "rate_limits": {"five_hour": {"utilization": 94, "resets_at": "2026-09-04T21:20:00+00:00"}}
        })))
        .unwrap();
    cx.run_until_seen(|view| view.providers().usage_checking.is_empty());
    let usage = &cx.seen().providers().provider_usage;
    assert_eq!(usage.keys().collect::<Vec<_>>(), ["claude"]);
    assert_eq!(usage["claude"].plan.as_deref(), Some("Ultra"));
    assert_eq!(usage["claude"].windows.len(), 1);
    assert_eq!(providers.count(), 1, "no other harness is started for it");
}

/// Another Claude account starts from a copy of the user's own configuration,
/// never the default account's sign-in, endpoint or history. A provider
/// without account sign-in is refused.
#[cfg(unix)]
#[test]
fn another_claude_account_inherits_configuration_but_not_the_account() {
    let test_store = TestStore::new("tcode-account-profile-test");
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
    let mut host = TestClientState::new((*test_store).clone());
    let claude = host.settings.provider_mut(ProviderKind::ClaudeCode);
    // The sign-in the new profile starts must not reach a `claude` on PATH.
    claude.binary_path = Some(test_store.root().join("no-claude"));
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
}

/// Another Codex account gets its own `CODEX_HOME` with a copy of the user's
/// configuration, never the default account's sign-in or sessions.
#[test]
fn another_codex_account_inherits_configuration_but_not_the_account() {
    let test_store = TestStore::new("tcode-codex-account-test");
    // The sign-in the new profile starts must not reach a `codex` on PATH.
    let binary = test_store.root().join("no-codex");
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
        )
        .unwrap();
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
    state.acp_installing.insert(
        "last".into(),
        acp::AcpInstall {
            operation: RuntimeOperationId(1),
            name: "Last".into(),
            cancelled: Default::default(),
        },
    );

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

/// A settings file this build cannot read — one a newer build wrote, opened
/// by an older install on the same machine — is never written over: the
/// host runs on defaults, every change it refuses says why, and the file
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
    assert_eq!(refusals, 2);
    state.update(cx, |state, _| state.close_store().unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
}

/// A deleted profile's id comes back with the next profile of that name, which
/// owns none of its stars; other accounts' stars and bare ids stay.
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

    state.dispatch_command(cx, 2, Command::DeleteProfile { profile_id: id });
    answered(cx, 2);
    assert!(!home.exists());
    assert_eq!(
        std::fs::read_to_string(left.join(".claude.json")).unwrap(),
        account
    );
    assert!(left.join("projects/-repo/session.jsonl").is_file());
}

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

        state.delete_profile("second-account", cx).detach();

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

/// A message goes to the provider with what the user attached to it: the
/// terminal lines they picked and their review comments, in that order after
/// the text, and the images it names; once sent, none of them waits for the
/// next message.
#[test]
fn send_turn_assembles_draft_context_and_attachment_paths() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-send-assembly-test");
    let root = test_store.root().clone();
    let attachment_path = root.join("sample.png");
    std::fs::write(&attachment_path, [1, 2, 3]).unwrap();
    stored_thread(
        &test_store,
        "assembled",
        ProviderKind::Codex,
        &root.join("project"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.open(cx, "assembled");
    state
        .command(
            cx,
            Command::ToggleTerminalPanel {
                session_id: "assembled".into(),
            },
        )
        .unwrap();
    cx.run_until_seen(|view| !view.status("assembled").terminals.is_empty());
    let terminal = cx.seen().status("assembled").terminals[0].clone();
    state
        .command(
            cx,
            Command::CaptureTerminalSelection {
                session_id: "assembled".into(),
                terminal_id: terminal.id,
                selection: Some(tcode_protocol::TerminalSelection {
                    line_start: 12,
                    line_end: 13,
                    text: "cargo test\nok".into(),
                }),
            },
        )
        .unwrap();
    for comment in [
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
    ] {
        state
            .command(
                cx,
                Command::AddReviewComment {
                    session_id: "assembled".into(),
                    comment,
                },
            )
            .unwrap();
    }
    state
        .command(
            cx,
            Command::SendTurn {
                session_id: "assembled".into(),
                text: "Explain this".into(),
                attachment_paths: vec![attachment_path.clone()],
            },
        )
        .unwrap();

    let SessionCommand::SendTurn {
        text, attachments, ..
    } = providers.started(cx, 0).next_command(cx)
    else {
        panic!("expected the message")
    };
    assert_eq!(
        text,
        format!(
            "Explain this\n\n<terminal_context>\n- {} lines 12-13:\n  12 | cargo test\n  13 | ok\n</terminal_context>\n\n<review_comment sectionId=\"section\" sectionTitle=\"Changes\" filePath=\"src/lib.rs\" startIndex=\"3\" endIndex=\"4\" rangeLabel=\"+7\">\nPlease fix\n```diff\nlet bad = true;\n```\n</review_comment>\n\n<review_comment sectionId=\"turn:3\" sectionTitle=\"Turn 4\" filePath=\"src/lib.rs\" startIndex=\"12\" endIndex=\"13\" rangeLabel=\"+7 to +8\">\nPlease avoid the unwrap.\n```diff\n@@ -7,1 +7,2 @@\n old\n+new\n```\n</review_comment>",
            terminal.title
        )
    );
    assert_eq!(
        attachments,
        vec![Attachment {
            media_type: "image/png".into(),
            data_base64: "AQID".into(),
            source_path: Some(attachment_path.to_string_lossy().into_owned()),
        }]
    );
    let status = cx.seen().status("assembled");
    assert!(status.terminal_contexts.is_empty());
    assert!(status.review_comment_drafts.is_empty());
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
fn draft_defaults_follow_history_within_current_account_and_model_settings() {
    let store = TestStore::new("tcode-draft-defaults");
    let project = Project::from_root(store.root().join("project"));
    let empty_project = Project::from_root(store.root().join("empty-project"));
    for project in [&project, &empty_project] {
        fs::create_dir_all(&project.root).unwrap();
        store.upsert_project(project).unwrap();
    }
    for (id, provider, project_id, model, updated_at) in [
        (
            "older",
            ProviderKind::Codex,
            project.id.as_str(),
            "older-model",
            100,
        ),
        (
            "project",
            ProviderKind::ClaudeCode,
            project.id.as_str(),
            "team-model",
            500,
        ),
        (
            "global",
            ProviderKind::Acp,
            "other-project",
            "global-model",
            900,
        ),
        (
            "archived",
            ProviderKind::Codex,
            project.id.as_str(),
            "archived-model",
            1000,
        ),
    ] {
        stored_thread(&store, id, provider, &project.root, |meta| {
            meta.project_id = Some(project_id.into());
            meta.model = Some(model.into());
            meta.updated_at = updated_at;
            meta.option_selections = vec![OptionSelection {
                id: agent::EFFORT_OPTION_ID.into(),
                value: serde_json::json!("high"),
            }];
            match id {
                "project" => {
                    meta.profile_id = Some("team".into());
                    meta.option_selections.extend([
                        OptionSelection {
                            id: "permissionMode".into(),
                            value: serde_json::json!("acceptEdits"),
                        },
                        OptionSelection {
                            id: "serviceTier".into(),
                            value: serde_json::json!("fast"),
                        },
                    ]);
                }
                "global" => meta.acp_agent_id = Some("global-agent".into()),
                "archived" => meta.archived_at = Some(1001),
                _ => {}
            }
        });
    }
    let mut settings = Settings {
        auto_settle_after_days: None,
        ..Settings::default()
    };
    settings.profiles.insert(
        "team".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                custom_models: vec!["team-model".into(), "team-next".into()],
                env: vec![tcode_core::settings::EnvVar {
                    name: "ANTHROPIC_API_KEY".into(),
                    value: String::new(),
                    sensitive: true,
                }],
                ..Default::default()
            },
        },
    );
    settings.acp_agents.insert(
        "global-agent".into(),
        InstalledAgent {
            id: "global-agent".into(),
            name: "Global agent".into(),
            version: String::new(),
            icon: None,
            launch: agent::AcpLaunch::Custom {
                command: "global-agent".into(),
                args: Vec::new(),
                env: Vec::new(),
            },
            enabled: true,
            env: Vec::new(),
            launch_args: None,
        },
    );
    tcode_services::settings::SettingsStore::new(store.root().clone())
        .save(&settings)
        .unwrap();
    store
        .save_models(
            ProviderKind::ClaudeCode,
            &[
                agent::ModelSpec {
                    id: "native-default".into(),
                    display_name: "Default".into(),
                    is_default: true,
                    options: Vec::new(),
                },
                agent::ModelSpec {
                    id: "native-other".into(),
                    display_name: "Other".into(),
                    is_default: false,
                    options: Vec::new(),
                },
            ],
        )
        .unwrap();
    let cx = &mut TestAppContext::default();
    let (state, _) = played_host(cx, &store);
    let start = |project: &Project, cwd: PathBuf| Command::StartDraft {
        project_id: project.id.clone(),
        cwd,
    };
    let draft = |response| match response {
        CommandResponse::SessionId(Some(id)) => id,
        other => panic!("draft response: {other:?}"),
    };
    let project_draft = draft(
        state
            .command(cx, start(&project, project.root.clone()))
            .unwrap(),
    );
    state.open(cx, &project_draft);
    let status = cx.seen().status(&project_draft);
    assert_eq!(status.provider, ProviderKind::ClaudeCode);
    assert_eq!(status.requested_profile_id.as_deref(), Some("team"));
    assert_eq!(status.requested_model.as_deref(), Some("team-model"));
    assert!(
        status
            .provider_option_requested_selections
            .iter()
            .any(|option| option.id == agent::EFFORT_OPTION_ID && option.value == "high")
    );
    assert!(
        status
            .provider_option_requested_selections
            .iter()
            .any(|option| option.id == "permissionMode" && option.value == "acceptEdits")
    );
    assert!(
        !status
            .provider_option_requested_selections
            .iter()
            .any(|option| option.id == "serviceTier")
    );
    let global_draft = draft(
        state
            .command(cx, start(&empty_project, empty_project.root.clone()))
            .unwrap(),
    );
    state.open(cx, &global_draft);
    let status = cx.seen().status(&global_draft);
    assert_eq!(status.provider, ProviderKind::Acp);
    assert_eq!(status.requested_model.as_deref(), Some("global-model"));
    assert_eq!(status.acp_agent_id.as_deref(), Some("global-agent"));
    assert!(
        !status
            .provider_option_requested_selections
            .iter()
            .any(|option| option.id == agent::EFFORT_OPTION_ID)
    );

    let patch = |profile: &str, patch| Command::UpdateProfileSettings {
        profile_id: profile.into(),
        patch,
    };
    state
        .command(
            cx,
            patch(
                "team",
                ProfileSettingsPatch::SetHiddenModels(vec!["team-model".into()]),
            ),
        )
        .unwrap();
    let reopened = draft(
        state
            .command(cx, start(&project, project.root.clone()))
            .unwrap(),
    );
    assert_eq!(reopened, project_draft);
    state.open(cx, &reopened);
    let status = cx.seen().status(&reopened);
    assert_eq!(status.requested_profile_id.as_deref(), Some("team"));
    assert_eq!(status.requested_model.as_deref(), Some("team-next"));
    state
        .command(
            cx,
            Command::UpdateAcpAgent {
                id: "global-agent".into(),
                patch: tcode_core::acp::AcpAgentPatch::SetEnabled { enabled: false },
            },
        )
        .unwrap();
    for provider in ProviderKind::NATIVE {
        state
            .command(
                cx,
                patch(
                    Settings::builtin_profile_id(provider),
                    ProfileSettingsPatch::SetEnabled { enabled: false },
                ),
            )
            .unwrap();
    }
    state
        .command(
            cx,
            patch(
                "team",
                ProfileSettingsPatch::SetHiddenModels(vec![
                    "team-model".into(),
                    "team-next".into(),
                ]),
            ),
        )
        .unwrap();
    for cwd in [project.root.clone(), project.root.join("fresh")] {
        let error = state.command(cx, start(&project, cwd)).unwrap_err();
        assert_eq!(error.code, "no_available_model");
    }
    state
        .command(
            cx,
            patch("claude", ProfileSettingsPatch::SetEnabled { enabled: true }),
        )
        .unwrap();
    state
        .command(
            cx,
            patch(
                "claude",
                ProfileSettingsPatch::SetHiddenModels(vec!["native-default".into()]),
            ),
        )
        .unwrap();
    state
        .command(
            cx,
            patch("team", ProfileSettingsPatch::SetEnabled { enabled: false }),
        )
        .unwrap();
    let fresh = draft(
        state
            .command(cx, start(&project, project.root.join("fresh")))
            .unwrap(),
    );
    state.open(cx, &fresh);
    let status = cx.seen().status(&fresh);
    assert_eq!(status.provider, ProviderKind::ClaudeCode);
    assert_eq!(status.requested_profile_id, None);
    assert_eq!(status.requested_model.as_deref(), Some("native-other"));
    state.command(cx, Command::ShutdownAllAndFlush).unwrap();
    let persisted = store.load_index().unwrap();
    for id in [&project_draft, &global_draft, &fresh] {
        assert!(!persisted.iter().any(|meta| &meta.id == id));
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

    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new(
        SessionStore::open_at(root.clone()).unwrap(),
    ));
    state.update(cx, |state, cx| {
        state.start_draft("project".into(), PathBuf::from("/tmp/project"), cx);
        let draft = state.selected_session().unwrap();
        assert_eq!(draft.meta.provider, ProviderKind::ClaudeCode);
        assert_eq!(draft.provider_commands, commands);
    });
}

/// Settings → Harnesses binary and launch arguments reach the spawn options.
#[test]
fn provider_binary_and_launch_arguments_follow_the_selected_profile() {
    let mut settings = Settings::default();
    let claude = settings.provider_mut(ProviderKind::ClaudeCode);
    claude.binary_path = Some(PathBuf::from("/custom/claude"));
    claude.launch_args = Some("--chrome --verbose".into());
    let codex = settings.provider_mut(ProviderKind::Codex);
    codex.binary_path = Some(PathBuf::from("/custom/codex"));
    let pi = settings.provider_mut(ProviderKind::Pi);
    pi.launch_args = Some("--verbose".into());

    let meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        McpServers::default(),
    )
    .unwrap();
    assert_eq!(opts.binary_path, Some(PathBuf::from("/custom/claude")));
    assert_eq!(opts.extra_args, vec!["--chrome", "--verbose"]);

    // This Codex profile has no launch arguments.
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        McpServers::default(),
    )
    .unwrap();
    assert_eq!(opts.binary_path, Some(PathBuf::from("/custom/codex")));
    assert!(opts.extra_args.is_empty());

    // pi project trust is opt-in and appends --approve after launch args.
    let meta = SessionMeta::new(ProviderKind::Pi, PathBuf::from("/x"), None);
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        McpServers::default(),
    )
    .unwrap();
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
    )
    .unwrap();
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
        state.settings_store.profile_secrets(&harness).unwrap(),
        state.settings_store.profile_secrets("klaude-kode").unwrap(),
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
        state.settings_store.profile_secrets(&harness).unwrap(),
        state
            .settings_store
            .profile_secrets(builtin_profile_id)
            .unwrap(),
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
    let opts = session_options(&meta, &state.settings, launch_env, McpServers::default()).unwrap();
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
            )
            .unwrap();
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

    // A fallback is labelled as such.
    for (output, expected_tail) in [
        (String::new(), String::from("(no assistant output)")),
        ("done".into(), "done".into()),
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

/// A subagent's report reaches its lead, and only the report: what else it
/// said stays in its thread, waking itself later tells the lead nothing, and
/// one that ends its turn silent is reminded once, visibly, and then its last
/// words go to the lead marked as a fallback. A report made after that still
/// reaches the lead, once. A thread no other thread started cannot report.
#[test]
fn only_report_result_reaches_the_lead_and_a_silent_child_is_reminded_then_fallen_back() {
    use tcode_core::notice::{ORCHESTRATE_FALLBACK_LABEL, ORCHESTRATE_REPORT_LABEL};
    use tcode_core::settlement::AgentReport;

    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-report-result-test");
    let fleet = Fleet::new(
        cx,
        &test_store,
        ProviderKind::Codex,
        &["reporter", "silent"],
        |_| {},
    );
    let report = |text: &str| serde_json::json!({ "text": text });
    assert!(
        fleet
            .lead
            .control(cx, "report_result", report("nope"))
            .is_err()
    );
    let reporter = fleet.child("reporter");
    assert_eq!(
        reporter
            .control(cx, "report_result", report("the full reported RESULT"))
            .unwrap()["delivered_when"],
        "turn_ends"
    );

    // The report rides in the child's own transcript as the tool call.
    reporter.play([
        persisted_assistant_event("working"),
        report_result_event("rep-1", "the full reported RESULT"),
        persisted_assistant_event("done"),
        completed_turn("turn-1"),
    ]);
    let silent = fleet.child("silent");
    silent.play([
        persisted_assistant_event("last message"),
        completed_turn("turn-1"),
    ]);
    // The silent child is reminded once, visibly, instead of being reported on.
    let (reminder, text) = silent.next_turn(cx);
    assert_eq!(text, CHILD_REPORT_REMINDER);
    let delivered = fleet.callbacks(cx, &["reporter", "silent"]);
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
    assert_eq!(fleet.report(cx, "reporter"), AgentReport::Reported);
    assert_eq!(fleet.report(cx, "silent"), AgentReport::Expected);

    // The reporter wakes itself (a monitor, a scheduled wake-up): nothing
    // new for the lead.
    reporter.play([
        AgentEvent::TurnStarted {
            turn_id: "turn-2".into(),
        },
        persisted_assistant_event("Four of 10 done."),
        completed_turn("turn-2"),
    ]);
    // The silent child runs the reminder turn and still does not report.
    silent.play([
        AgentEvent::TurnAccepted {
            delivery_id: reminder,
        },
        AgentEvent::TurnStarted {
            turn_id: "turn-2".into(),
        },
        persisted_assistant_event("still nothing"),
        completed_turn("turn-2"),
    ]);
    let delivered = fleet.callbacks(cx, &["reporter", "silent"]);
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert!(delivered[0].starts_with("[orchestrate] thread silent (\""));
    assert!(delivered[0].contains(&format!(
        "unreported.\n{ORCHESTRATE_FALLBACK_LABEL}still nothing"
    )));
    assert_eq!(fleet.report(cx, "silent"), AgentReport::Unreported);
    assert!(
        next_command(&silent.commands).is_err(),
        "no second reminder"
    );

    // A late report still reaches the lead, once.
    silent.play([AgentEvent::TurnStarted {
        turn_id: "turn-3".into(),
    }]);
    silent
        .control(cx, "report_result", report("late report"))
        .unwrap();
    silent.play([
        report_result_event("rep-2", "late report"),
        completed_turn("turn-3"),
        AgentEvent::TurnStarted {
            turn_id: "turn-4".into(),
        },
        completed_turn("turn-4"),
    ]);
    let delivered = fleet.callbacks(cx, &["silent"]);
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert!(delivered[0].contains(&format!("reported.\n{ORCHESTRATE_REPORT_LABEL}late report")));
}

/// A lead whose turn runs, and the subagents it sent their brief to through
/// `tcode_control`, each running the turn that brief opened and shown on the
/// client. Every thread was stored before the host started, the subagents
/// under the lead, each working in a directory of its own.
struct Fleet {
    state: TestEntity,
    providers: Providers,
    lead: Played,
    children: HashMap<String, Played>,
    root: PathBuf,
}

impl Fleet {
    fn new(
        cx: &mut TestAppContext,
        store: &SessionStore,
        provider: ProviderKind,
        children: &[&str],
        edit: impl Fn(&mut SessionMeta),
    ) -> Self {
        let root = store.root().clone();
        stored_thread(store, "parent", provider, &root.join("parent"), &edit);
        for child in children {
            stored_thread(store, child, provider, &root.join(child), |meta| {
                meta.parent_session_id = Some("parent".into());
                edit(meta);
            });
        }
        let (state, providers) = controlled_host(cx, store);
        state.subscribe(cx, Topic::Index);
        let lead = first_turn(&state, cx, &providers, "parent", 0);
        let mut fleet = Self {
            state,
            providers,
            lead,
            children: HashMap::new(),
            root,
        };
        if !children.is_empty() {
            let sent = fleet.dispatch(cx, children, "Do your part");
            for child in children {
                fleet.state.open(cx, child);
                let played = fleet.providers.started_in(cx, &fleet.root.join(child));
                played.start_turn(cx, "turn-1");
                cx.run_until_seen(|view| view.status(child).running_turn.is_some());
                fleet.children.insert(child.to_string(), played);
            }
            assert!(
                sent["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|result| result["outcome"] == "applied"),
                "{sent}"
            );
        }
        fleet
    }

    fn child(&self, id: &str) -> &Played {
        &self.children[id]
    }

    /// The lead sends `children` `text` after their running turn.
    fn dispatch(
        &self,
        cx: &mut TestAppContext,
        children: &[&str],
        text: &str,
    ) -> serde_json::Value {
        self.lead
            .control(
                cx,
                "send_message",
                serde_json::json!({"thread_ids": children, "text": text, "mode": "queue"}),
            )
            .unwrap()
    }

    /// What the lead is told of its subagents, steered into its running
    /// turn, until the host has taken everything `children` said so far.
    fn callbacks(&self, cx: &mut TestAppContext, children: &[&str]) -> Vec<String> {
        for child in children {
            self.child(child).fence(cx, child);
        }
        cx.run_until_parked();
        std::iter::from_fn(|| next_command(&self.lead.commands).ok())
            .map(|command| match command {
                SessionCommand::Steer { text, .. } => text,
                other => panic!("the lead was sent {other:?}"),
            })
            .collect()
    }

    /// The subagent's report as the index gives it.
    fn report(&self, cx: &TestAppContext, child: &str) -> tcode_core::settlement::AgentReport {
        cx.seen().activity(child).unwrap().agent.unwrap().report
    }
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
            family: None,
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

/// The log Tcode stored for a dispatched Claude Code child, in the order a
/// real one ran (October 2026) and with its content made up: its brief ran a
/// background script, the Monitor tool woke it nine times with one-line
/// progress messages (turns 2–10), and it reported once, in turn 12. Its lead
/// received twelve "completed" callbacks. The contract is
/// one: the report. `BackgroundTasksChanged` is not persisted, so the Monitor's
/// liveness is played the way the CLI publishes it — set when the monitor
/// starts, cleared with the result of the turn in which it ended. The brief
/// and its turn's start are the host's own records, so the provider plays
/// what follows them.
#[test]
fn a_child_waking_itself_reports_to_its_lead_once() {
    use tcode_core::session::StoredEvent;
    use tcode_core::settlement::AgentReport;

    const RECORDING: &str = include_str!("fixtures/monitor_wakeups_child.jsonl");

    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-monitor-wakeups");
    let fleet = Fleet::new(
        cx,
        &test_store,
        ProviderKind::ClaudeCode,
        &["child"],
        |meta| {
            if meta.id == "child" {
                meta.title = "改写样例说明（10 条）".into();
            }
        },
    );
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
    let child = fleet.child("child");
    for record in records
        .into_iter()
        .skip_while(|record| !matches!(&record.event, AgentEvent::TurnStarted { .. }))
        .skip(1)
    {
        let monitor_started = matches!(
            &record.event,
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::ToolCall { name, .. },
                ..
            }) if name == "Monitor"
        );
        if matches!(&record.event, AgentEvent::TurnCompleted { turn_id, .. } if *turn_id == last_turn)
        {
            child.play([AgentEvent::BackgroundTasksChanged { count: 0 }]);
        }
        child.play([record.event]);
        if monitor_started {
            child.play([AgentEvent::BackgroundTasksChanged { count: 1 }]);
        }
    }

    let callbacks = fleet.callbacks(cx, &["child"]);
    assert_eq!(
        callbacks.len(),
        1,
        "one report, one callback: {callbacks:?}"
    );
    assert!(
        callbacks[0]
            .starts_with("[orchestrate] thread child (\"改写样例说明（10 条）\") reported.")
    );
    assert!(callbacks[0].ends_with("\n<report>"));
    assert!(
        !callbacks.iter().any(|text| text.contains("of 10 done")),
        "progress lines stay in the child"
    );
    assert!(
        std::iter::from_fn(|| next_command(&child.commands).ok())
            .all(|command| !matches!(command, SessionCommand::SendTurn { .. })),
        "a child waiting on its monitor is not reminded"
    );
    assert_eq!(fleet.report(cx, "child"), AgentReport::Reported);
}

/// A follow-up to a subagent whose log is not loaded yet is owed a new report: the report
/// the subagent made for the previous message is not delivered again as its answer, and
/// the subagent is reminded to make one.
#[test]
fn a_follow_up_to_an_unloaded_subagent_is_not_answered_by_its_earlier_report() {
    use tcode_core::settlement::AgentReport;

    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-subagent-follow-up-unloaded");
    let cwd = store.root().join("child");
    stored_thread(&store, "child", ProviderKind::Codex, &cwd, |child| {
        child.parent_session_id = Some("parent".into());
        child.expectation = Some(ChildExpectation {
            state: AgentReport::Reported,
            delivered_item: Some("rep-old".into()),
            ..ChildExpectation::opened(1)
        });
    });
    let earlier = cx.now_millis();
    cx.clock.advance(Duration::from_secs(60));
    for event in [
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        report_result_event("rep-old", "the earlier report"),
        completed_turn("turn-1"),
    ] {
        store.append_event("child", earlier, &event).unwrap();
    }
    let fleet = Fleet::new(cx, &store, ProviderKind::Codex, &[], |_| {});
    let sent = fleet.dispatch(cx, &["child"], "One more check");
    assert_eq!(sent["results"][0]["outcome"], "applied", "{sent}");
    let child = fleet.providers.started_in(cx, &cwd);
    child.start_turn(cx, "turn-2");
    child.play([
        persisted_assistant_event("checked, no report"),
        completed_turn("turn-2"),
    ]);
    assert_eq!(child.next_turn(cx).1, CHILD_REPORT_REMINDER);
    cx.run_until_parked();
    assert!(
        next_command(&fleet.lead.commands).is_err(),
        "the lead is told nothing"
    );
    assert_eq!(fleet.report(cx, "child"), AgentReport::Expected);
}

/// An orchestrator returning to a long-idle child whose worktree has since been
/// removed must hear that the child could not start and why, not nothing (the
/// failure folds into the already-reported turn) or that turn's old output.
#[test]
fn send_to_child_whose_cwd_was_removed_reports_the_start_failure() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-send-missing-cwd-test");
    let cwd = test_store.root().join("child");
    std::fs::create_dir_all(&cwd).unwrap();
    run_git(&cwd, &["init", "-b", "main"]).unwrap();
    let fleet = Fleet::new(cx, &test_store, ProviderKind::Codex, &["child"], |meta| {
        meta.project_id = Some("project".into());
    });
    // Windows refuses to remove a directory a process is working in, and the
    // host runs git in the child's directory when it starts and when a turn
    // ends; in use, the idle grace outlasts both. The read at the start sees
    // a clean tree, the one at the turn's end the child's work.
    cx.run_until(|state| state.git_status.contains_key("child"));
    std::fs::write(cwd.join("part.txt"), "done\n").unwrap();
    let child = fleet.child("child");
    child.play([
        report_result_event("rep-1", "old report"),
        completed_turn("turn-1"),
    ]);
    assert!(fleet.lead.next_steer(cx).ends_with("\nold report"));
    cx.run_until(|state| {
        matches!(
            state.git_status.get("child"),
            Some(Ok(status)) if status.has_working_tree_changes
        )
    });

    // Its harness has been idle for as long as the host keeps one.
    cx.clock.advance(RESIDENT_IDLE_GRACE);
    assert!(matches!(child.next_command(cx), SessionCommand::Shutdown));
    std::fs::remove_dir_all(&cwd).unwrap();
    // What `agent::start_session` says of a directory that is gone.
    fleet.providers.refuse_in(
        &cwd,
        format!("working directory `{}` no longer exists", cwd.display()),
    );
    cx.drain_outgoing();
    let sent = fleet.dispatch(cx, &["child"], "continue");
    assert_eq!(sent["results"][0]["outcome"], "applied", "{sent}");
    let failed = fleet.lead.next_steer(cx);
    assert_eq!(
        failed,
        format!(
            "[orchestrate] thread child (\"{}\") failed.\nfailed to spawn provider process: working directory `{}` no longer exists",
            cx.seen().session("child").unwrap().title,
            cwd.display()
        )
    );
    assert!(
        cx.seen()
            .notifications()
            .iter()
            .any(|notification| matches!(
                notification,
                RuntimeEvent::Error(RuntimeError::WorkingDirectoryMissing {
                    session_id,
                    project_id: Some(project_id),
                    cwd: missing,
                }) if session_id == "child" && project_id == "project" && missing == &cwd
            )),
        "a missing working directory is reported with its project so a client can offer to re-point it"
    );
    let thread = fleet
        .lead
        .control(
            cx,
            "get_thread",
            serde_json::json!({ "thread_id": "child" }),
        )
        .unwrap();
    assert_eq!(thread["report"], "failed", "{thread}");
}

/// A subagent's approval request reaches its lead with the harness's own
/// options, and the lead answers with one of those ids, or cancels, unless
/// the user keeps subagent approvals to themselves; only the lead may answer,
/// one request at a time, with what the request offered. With approvals left
/// to the agents, the harness's own "allow" answers on the spot, the most
/// lasting one first.
#[test]
fn child_native_approval_callback_and_respond_approval_preserve_options_and_scope() {
    use agent::ApprovalOptionKind::{AllowAlways, AllowOnce, RejectOnce};
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-native-child-approvals");
    stored_thread(
        &store,
        "other-parent",
        ProviderKind::Codex,
        &store.root().join("other"),
        |_| {},
    );
    let fleet = Fleet::new(cx, &store, ProviderKind::Codex, &["child"], |_| {});
    let other = first_turn(&fleet.state, cx, &fleet.providers, "other-parent", 2);
    let (state, child) = (&fleet.state, fleet.child("child"));
    let set_mode = |cx: &mut TestAppContext, mode| {
        state
            .command(
                cx,
                Command::PatchSettings {
                    patch: SettingsPatch::OrchestrateChildApproval(mode),
                },
            )
            .unwrap();
    };
    let respond = |cx: &mut TestAppContext,
                   caller: &Played,
                   approval: Option<&str>,
                   option: Option<&str>,
                   cancel: bool| {
        caller.control(
            cx,
            "respond_approval",
            serde_json::json!({
                "thread_id": "child",
                "approval_id": approval,
                "option": option,
                "cancel": cancel,
            }),
        )
    };
    let pending = |cx: &TestAppContext| cx.seen().status("child").pending_approvals.len();
    for (n, mode) in [
        ChildApprovalMode::Orchestrator,
        ChildApprovalMode::Auto,
        ChildApprovalMode::Manual,
        ChildApprovalMode::AlwaysAllow,
    ]
    .into_iter()
    .enumerate()
    {
        set_mode(cx, mode);
        let request_id = format!("request-{n}");
        let options = if mode == ChildApprovalMode::AlwaysAllow {
            vec![agent::ApprovalOption {
                id: "Reject:Native".into(),
                label: "Reject".into(),
                kind: RejectOnce,
            }]
        } else {
            vec![
                agent::ApprovalOption {
                    id: "Allow:Native".into(),
                    label: "Native allowance".into(),
                    kind: AllowOnce,
                },
                agent::ApprovalOption {
                    id: "Reject:Native".into(),
                    label: "Reject".into(),
                    kind: RejectOnce,
                },
            ]
        };
        child.play([AgentEvent::ApprovalRequested(agent::ApprovalRequest {
            parent_item_id: None,
            id: request_id.clone(),
            turn_id: None,
            kind: agent::ApprovalKind::ToolUse {
                name: "mcp__tcode_control__report_result".into(),
                input: serde_json::json!({}),
                detail: "Native MCP consent".into(),
            },
            reason: None,
            blocked_path: None,
            options: options.clone(),
        })]);
        let text = fleet.lead.next_steer(cx);
        assert!(
            next_command(&child.commands).is_err(),
            "requests without automatic allow must remain pending"
        );
        assert!(text.contains(&request_id));
        let native = text
            .split_once("Native options: ")
            .unwrap()
            .1
            .split_once(". ")
            .unwrap()
            .0;
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
            (
                &other,
                None,
                Some("Reject:Native"),
                false,
                "not your subagent",
            ),
            (
                &fleet.lead,
                Some("missing"),
                Some("Reject:Native"),
                false,
                "is not pending",
            ),
            (
                &fleet.lead,
                None,
                Some("invented-id"),
                false,
                "valid option ids",
            ),
            (&fleet.lead, None, Some("Reject:Native"), true, "never both"),
            (&fleet.lead, None, None, false, "never both"),
        ];
        // In manual mode the approval is the user's, whatever the lead asks.
        let manual_refusal = [(
            &fleet.lead,
            None,
            Some("Reject:Native"),
            false,
            "subagent approvals is manual",
        )];
        for (caller, approval, option, cancel, error) in if manual {
            &manual_refusal[..]
        } else {
            &refusals[..]
        } {
            let refused = respond(cx, caller, *approval, *option, *cancel).unwrap_err();
            assert!(refused.contains(error), "{refused}");
            assert!(next_command(&child.commands).is_err());
            assert_eq!(pending(cx), 1);
        }
        let chosen = offered.first().unwrap().id.clone();
        if manual {
            state
                .command(
                    cx,
                    Command::RespondApproval {
                        session_id: "child".into(),
                        request_id: request_id.clone(),
                        decision: ApprovalDecision::Cancel,
                    },
                )
                .unwrap();
        } else {
            assert_eq!(
                respond(cx, &fleet.lead, None, Some(&chosen), false).unwrap()["approval_id"],
                request_id
            );
        }
        let SessionCommand::RespondApproval {
            request_id: delivered,
            decision,
        } = child.next_command(cx)
        else {
            panic!("missing native approval response")
        };
        assert_eq!(delivered, request_id);
        assert_eq!(
            decision,
            if manual {
                ApprovalDecision::Cancel
            } else {
                ApprovalDecision::Option(chosen)
            }
        );
        cx.run_until_parked();
        assert_eq!(pending(cx), 0);
    }
    set_mode(cx, ChildApprovalMode::AlwaysAllow);
    for (n, kinds, expected) in [
        (
            0,
            vec![RejectOnce, AllowOnce, AllowAlways, AllowOnce],
            "native-2",
        ),
        (1, vec![RejectOnce, AllowOnce], "native-1"),
    ] {
        let request_id = format!("automatic-{n}");
        child.play([AgentEvent::ApprovalRequested(agent::ApprovalRequest {
            parent_item_id: None,
            id: request_id.clone(),
            turn_id: None,
            kind: agent::ApprovalKind::ExecCommand {
                command: "cargo check".into(),
                cwd: None,
                description: None,
            },
            reason: None,
            blocked_path: None,
            options: kinds
                .into_iter()
                .enumerate()
                .map(|(i, kind)| agent::ApprovalOption {
                    id: format!("native-{i}"),
                    label: "Native choice".into(),
                    kind,
                })
                .collect(),
        })]);
        assert!(matches!(
            child.next_command(cx),
            SessionCommand::RespondApproval {
                request_id: delivered,
                decision: ApprovalDecision::Option(option),
            } if delivered == request_id && option == expected
        ));
        child.fence(cx, "child");
        assert!(next_command(&fleet.lead.commands).is_err());
        assert_eq!(pending(cx), 0);
    }
}

/// A `tcode_control` call made during a member's turn stays inside the member's space and
/// its policy, whatever the thread's grant reaches; the owner's own turn keeps the grant.
#[test]
fn control_calls_in_a_members_turn_stay_inside_the_space() {
    use tcode_core::control::{ControlCategory, ControlGrant, ControlScope, ControlTier};
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-space");
    let root = store.root().join("project-a");
    let mut project = tcode_core::project::Project::from_root(root.clone());
    project.id = "a".into();
    store.upsert_project(&project).unwrap();
    stored_thread(&store, "caller", ProviderKind::Codex, &root, |meta| {
        meta.project_id = Some("a".into());
        meta.control_grant = Some(ControlGrant {
            scope: ControlScope::Host,
            tiers: ControlTier::ALL.into_iter().collect(),
            categories: ControlCategory::ALL.into_iter().collect(),
        });
    });
    for (id, project) in [("same", "a"), ("other", "b")] {
        stored_thread(&store, id, ProviderKind::Codex, &root, |meta| {
            meta.project_id = Some(project.into());
        });
    }
    let (state, providers) = controlled_host(cx, &store);
    let member = |policy_revision, policy| tcode_protocol::Principal::Space {
        policy_revision,
        space_id: "s".into(),
        space_name: "Shared".into(),
        project_ids: vec!["a".into()],
        device_id: "phone".into(),
        device_name: "Phone".into(),
        policy,
    };
    let shared = || tcode_protocol::SpacePolicy {
        git_remote: false,
        ..tcode_protocol::SpacePolicy::unrestricted()
    };
    state
        .query_as(cx, member(1, shared()), tcode_protocol::Query::Ping)
        .unwrap();
    let get = |id: &str| serde_json::json!({ "thread_id": id });
    let push = serde_json::json!({ "thread_id": "same", "op": "push" });
    let settings = || serde_json::json!({});

    // The owner's own turn.
    let caller = first_turn(&state, cx, &providers, "caller", 0);
    assert!(caller.control(cx, "get_thread", get("other")).is_ok());
    assert!(caller.control(cx, "get_settings", settings()).is_ok());
    caller.play([completed_turn("turn")]);
    cx.run_until_seen(|view| view.status("caller").running_turn.is_none());

    // A member sends the turn the agent works in, as the pipe admits it.
    state
        .command_as(
            cx,
            member(1, shared()),
            Command::SendTurn {
                session_id: "caller".into(),
                text: "go".into(),
                attachment_paths: Vec::new(),
            },
        )
        .unwrap();
    caller.start_turn(cx, "member-turn");
    cx.run_until_seen(|view| view.status("caller").running_turn.is_some());
    assert!(caller.control(cx, "get_thread", get("same")).is_ok());
    let refused = caller.control(cx, "get_thread", get("other")).unwrap_err();
    assert!(refused.contains("outside"), "{refused}");
    let refused = caller.control(cx, "git", push).unwrap_err();
    assert!(refused.contains("pushing and pulling"), "{refused}");
    let refused = caller.control(cx, "get_settings", settings()).unwrap_err();
    assert!(refused.contains("in this space"), "{refused}");

    // What the member's turn sets going works for the member too: a
    // subagent it starts and a message it queues in another thread wait
    // once the space allows nothing. Their providers are still starting.
    providers.hold();
    let started = caller
        .control(
            cx,
            "start_thread",
            serde_json::json!({
                "subagent": true,
                "provider": "codex",
                "message": "work",
                "request_id": "child",
            }),
        )
        .unwrap();
    let child = started["thread_id"].as_str().unwrap().to_owned();
    caller
        .control(
            cx,
            "send_message",
            serde_json::json!({ "thread_ids": ["same"], "text": "more", "mode": "queue" }),
        )
        .unwrap();
    state
        .query_as(
            cx,
            member(2, tcode_protocol::SpacePolicy::new_space()),
            tcode_protocol::Query::Ping,
        )
        .unwrap();
    for id in [child.as_str(), "same"] {
        state.open(cx, id);
        let status = cx.seen().status(id);
        assert!(
            status
                .queued_messages
                .iter()
                .any(|message| message.space_held),
            "{id}: {:?}",
            status.queued_messages
        );
    }
    providers.let_go();
}

/// A subagent's question reaches its lead with the harness's own questions
/// and says whether the subagent waits; the lead answers each question, or
/// the user does when they keep subagent answers to themselves. Only the lead
/// answers its own subagent unasked, a pending request, every question and
/// no other; a question is never answered automatically.
#[test]
fn child_questions_reach_the_lead_and_respond_input_answers_them() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-child-questions");
    stored_thread(
        &store,
        "other-parent",
        ProviderKind::Codex,
        &store.root().join("other"),
        |_| {},
    );
    let fleet = Fleet::new(cx, &store, ProviderKind::Codex, &["child"], |_| {});
    let other = first_turn(&fleet.state, cx, &fleet.providers, "other-parent", 2);
    let (state, child) = (&fleet.state, fleet.child("child"));
    let respond = |cx: &mut TestAppContext,
                   caller: &Played,
                   question: Option<&str>,
                   answers: serde_json::Value| {
        caller.control(
            cx,
            "respond_input",
            serde_json::json!({
                "thread_id": "child",
                "question_id": question,
                "answers": answers,
            }),
        )
    };
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
        state
            .command(
                cx,
                Command::PatchSettings {
                    patch: SettingsPatch::OrchestrateChildApproval(mode),
                },
            )
            .unwrap();
        let request_id = format!("question-{n}");
        child.play([AgentEvent::UserInputRequested {
            request_id: request_id.clone(),
            questions: questions.clone(),
            delivery,
        }]);
        let text = fleet.lead.next_steer(cx);
        assert!(
            next_command(&child.commands).is_err(),
            "a question is never answered automatically"
        );
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
        for (caller, request, answers, error) in [
            (
                &other,
                None,
                serde_json::json!({"lang": "Rust", "targets": []}),
                "needs category answer_input",
            ),
            (
                &fleet.lead,
                Some("missing"),
                serde_json::json!({"lang": "Rust", "targets": []}),
                "request missing is not pending",
            ),
            (
                &fleet.lead,
                None,
                serde_json::json!({"lang": "Rust", "colour": "red"}),
                "unknown question id",
            ),
            (
                &fleet.lead,
                None,
                serde_json::json!({"lang": "Rust"}),
                "missing answers for question ids [\"targets\"]",
            ),
        ] {
            let refused = respond(cx, caller, request, answers).unwrap_err();
            assert!(refused.contains(error), "{refused}");
            assert!(next_command(&child.commands).is_err());
        }
        let answers = serde_json::json!({"lang": "Rust", "targets": ["ios", "android"]});
        assert_eq!(
            respond(cx, &fleet.lead, Some(&request_id), answers.clone()).unwrap()["request_id"],
            request_id
        );
        let SessionCommand::RespondUserInput {
            request_id: delivered,
            answers: sent,
        } = child.next_command(cx)
        else {
            panic!("missing answer")
        };
        assert_eq!(delivered, request_id);
        assert_eq!(serde_json::Value::Object(sent.clone()), answers);
        child.play([AgentEvent::UserInputResolved {
            request_id,
            answers: sent,
        }]);
        cx.run_until_seen(|view| view.status("child").pending_user_input.is_none());
        assert!(
            respond(cx, &fleet.lead, None, serde_json::json!({}))
                .unwrap_err()
                .contains("has no pending user-input request")
        );
    }
}

/// A subagent that ends silent after its reminder has its last turn's final
/// message, whole, go to the lead as the fallback: the adjacent blocks of it,
/// without the earlier preamble or the reasoning.
#[test]
fn an_unreported_fallback_is_the_final_message_of_the_last_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-fallback-final-message-test");
    let fleet = Fleet::new(cx, &test_store, ProviderKind::Codex, &["child"], |_| {});
    let child = fleet.child("child");
    child.play([completed_turn("turn-1")]);
    let (reminder, text) = child.next_turn(cx);
    assert_eq!(text, CHILD_REPORT_REMINDER);
    child.play([
        AgentEvent::TurnAccepted {
            delivery_id: reminder,
        },
        AgentEvent::TurnStarted {
            turn_id: "turn-2".into(),
        },
    ]);
    let report = format!("Complete answer.\n{}", "full detail ".repeat(80));
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
        child.play([AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        })]);
    }
    child.play([completed_turn("turn-2")]);
    let text = fleet.lead.next_steer(cx);
    assert!(
        text.ends_with(&format!(
            "{}{report}",
            tcode_core::notice::ORCHESTRATE_FALLBACK_LABEL
        )),
        "{text}"
    );
}

/// A subagent's report steered into its lead's running turn, the lead's
/// thread not shown anywhere, is recorded under the id the provider was sent,
/// so the provider's answer finds it.
#[test]
fn steering_parked_orchestrator_callback_uses_recorded_id() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-steer-test");
    let fleet = Fleet::new(cx, &test_store, ProviderKind::Codex, &["child-a"], |_| {});
    fleet.state.leave(cx, "parent");
    fleet.child("child-a").play([
        report_result_event("rep-1", "full result"),
        completed_turn("turn-1"),
    ]);
    let SessionCommand::Steer {
        request_id, text, ..
    } = fleet.lead.next_command(cx)
    else {
        panic!("callback did not steer")
    };
    assert!(text.contains("full result"));
    fleet.state.open(cx, "parent");
    assert!(
        cx.seen()
            .timeline("parent")
            .entries
            .iter()
            .any(|entry| matches!(
                &entry.content,
                EntryContent::Steer {
                    text,
                    status: tcode_core::session::SteeringStatus::Pending,
                    ..
                } if entry.id == request_id && text.contains("thread child-a")
            ))
    );
}

/// A user's steer, and a queued message the user steers, are sent under an
/// id their client's request names and recorded under the same id; a message
/// the host is already delivering as a turn is neither steered nor dropped.
#[test]
fn steering_user_and_queue_paths_send_the_same_id_they_record() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-user-steer-id-test");
    stored_thread(
        &test_store,
        "active",
        ProviderKind::Codex,
        &test_store.root().join("active"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let provider = first_turn(&state, cx, &providers, "active", 0);
    let queued = |cx: &TestAppContext| {
        cx.seen()
            .status("active")
            .queued_messages
            .iter()
            .map(|message| (message.id, message.text.clone()))
            .collect::<Vec<_>>()
    };
    let send = |cx: &mut TestAppContext, text: &str, key: &str| {
        state
            .command_keyed(
                cx,
                key,
                Command::SendTurn {
                    session_id: "active".into(),
                    text: text.into(),
                    attachment_paths: Vec::new(),
                },
            )
            .unwrap();
        queued(cx).last().unwrap().0
    };
    let steered = |cx: &TestAppContext, request_id: &str, text: &str| {
        cx.seen().timeline("active").entries.iter().any(|entry| {
            matches!(
                &entry.content,
                EntryContent::Steer {
                    text: steered,
                    status: tcode_core::session::SteeringStatus::Pending,
                    ..
                } if entry.id == request_id && steered == text
            )
        })
    };

    let sentinel = send(cx, "queued sentinel", "sentinel-send");
    state
        .command_keyed(
            cx,
            "steer-send",
            Command::Steer {
                session_id: "active".into(),
                text: "redirect".into(),
                attachment_paths: Vec::new(),
            },
        )
        .unwrap();
    let SessionCommand::Steer { request_id, .. } = provider.next_command(cx) else {
        panic!("user steer command missing")
    };
    assert_eq!(request_id, "local-steer-steer-send");
    cx.run_until_seen(|view| {
        has_record(view, "active", |event| {
            matches!(event, AgentEvent::SteerRequested { .. })
        })
    });
    assert!(steered(cx, &request_id, "redirect"));

    state
        .command(
            cx,
            Command::DropQueued {
                session_id: "active".into(),
                id: sentinel,
            },
        )
        .unwrap();
    let first = send(cx, "first", "first-send");
    let queued_id = send(cx, "queued redirect", "queue-send");
    let third = send(cx, "third", "third-send");
    state
        .command_keyed(
            cx,
            "queue-steer-click",
            Command::SteerQueued {
                session_id: "active".into(),
                id: queued_id,
            },
        )
        .unwrap();
    let SessionCommand::Steer { request_id, .. } = provider.next_command(cx) else {
        panic!("queue-to-steer command missing")
    };
    assert_eq!(request_id, "local-steer-queue-send");
    cx.run_until_seen(|view| {
        has_record(view, "active", |event| {
            matches!(event, AgentEvent::SteerRequested { request_id: recorded, .. } if *recorded == request_id)
        })
    });
    assert_eq!(
        queued(cx),
        [(first, "first".to_string()), (third, "third".to_string())]
    );
    assert!(steered(cx, &request_id, "queued redirect"));

    state
        .command(
            cx,
            Command::DropQueued {
                session_id: "active".into(),
                id: first,
            },
        )
        .unwrap();
    // The turn ends, and the host delivers the next message as a turn.
    provider.play([completed_turn("turn")]);
    let (delivery_id, text) = provider.next_turn(cx);
    assert_eq!((delivery_id, text.as_str()), (third, "third"));
    for command in [
        Command::DropQueued {
            session_id: "active".into(),
            id: third,
        },
        Command::SteerQueued {
            session_id: "active".into(),
            id: third,
        },
    ] {
        let _ = state.command(cx, command);
    }
    assert_eq!(queued(cx)[0].0, third);
    assert!(
        next_command(&provider.commands).is_err(),
        "an unacknowledged delivery cannot be steered twice"
    );
}

/// The runtime routes a steer by its own turn flag, but the provider's
/// `turn/completed` can already be on the wire. The provider then rejects the
/// steer by id, and the text goes out as the next ordinary turn, from whoever
/// sent it, instead of vanishing behind a notice.
#[test]
fn steer_rejected_after_turn_end_is_resent_as_a_new_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-steer-rejected-test");
    for id in ["active", "sender"] {
        stored_thread(
            &test_store,
            id,
            ProviderKind::Codex,
            &test_store.root().join(id),
            |meta| {
                meta.project_id = Some("project".into());
            },
        );
    }
    let (state, providers) = controlled_host(cx, &test_store);
    let active = first_turn(&state, cx, &providers, "active", 0);
    // Another thread's agent steers it.
    first_turn(&state, cx, &providers, "sender", 1)
        .control(
            cx,
            "send_message",
            serde_json::json!({ "thread_ids": ["active"], "text": "redirect", "mode": "steer" }),
        )
        .unwrap();
    let SessionCommand::Steer { request_id, .. } = active.next_command(cx) else {
        panic!("steer command missing")
    };

    active.play([completed_turn("turn-1")]);
    active.fence(cx, "active");
    assert!(
        next_command(&active.commands).is_err(),
        "nothing queued yet"
    );

    active.play([AgentEvent::SteerRejected {
        request_id: request_id.clone(),
    }]);
    let (delivery_id, text) = active.next_turn(cx);
    assert_eq!(text, "redirect");
    assert!(
        !cx.seen()
            .timeline("active")
            .entries
            .iter()
            .any(|entry| entry.id == request_id),
        "the pending steer bubble is gone; the send records its own message"
    );
    active.play([AgentEvent::TurnAccepted { delivery_id }]);
    cx.run_until_seen(|view| {
        view.records("active").iter().any(|record| {
            record.origin == Some(MessageOrigin::Agent)
                && matches!(
                    &record.event,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::UserMessage { text, .. },
                        ..
                    }) if text == "redirect"
                )
        })
    });

    active.play([AgentEvent::SteerRejected {
        request_id: request_id.clone(),
    }]);
    active.fence(cx, "active");
    assert!(
        next_command(&active.commands).is_err(),
        "a rejection for an already resolved steer sends nothing"
    );
}

/// Reports that reach a lead whose provider is still starting wait for it,
/// and wake it with one turn that carries all of them.
#[test]
fn callbacks_racing_provider_start_share_one_wakeup_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-start-test");
    let fleet = Fleet::new(
        cx,
        &test_store,
        ProviderKind::ClaudeCode,
        &["child-a", "child-b"],
        |_| {},
    );
    // The lead's turn ends and its harness exits; reports start it again.
    fleet.lead.play([
        completed_turn("turn"),
        AgentEvent::SessionClosed { reason: None },
    ]);
    cx.run_until_seen(|view| {
        has_record(view, "parent", |event| {
            matches!(event, AgentEvent::SessionClosed { .. })
        })
    });
    fleet.providers.hold();
    for (child, result) in [("child-a", "result a"), ("child-b", "result b")] {
        fleet.child(child).play([
            report_result_event(&format!("{child}-report"), result),
            completed_turn("turn-1"),
        ]);
        fleet.child(child).fence(cx, child);
    }
    let restarted = fleet.providers.started(cx, 3);
    assert_eq!(restarted.options.cwd, fleet.root.join("parent"));
    assert_eq!(fleet.providers.count(), 4, "one start for both");
    fleet.providers.let_go();
    let (delivery_id, text) = restarted.next_turn(cx);
    assert!(
        text.contains("result a") && text.contains("result b"),
        "{text}"
    );
    restarted.play([AgentEvent::TurnAccepted { delivery_id }]);
    restarted.fence(cx, "parent");
    assert!(
        next_command(&restarted.commands).is_err(),
        "one turn wakes it"
    );
}

/// Queued messages go in the order the user last moved them to.
#[test]
fn moved_queue_entries_dispatch_in_their_new_order() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-moved-queue-test");
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    for text in ["first", "second", "third"] {
        state.send(cx, "thread", text);
    }
    let id = |cx: &TestAppContext, text: &str| {
        cx.seen()
            .status("thread")
            .queued_messages
            .iter()
            .find(|message| message.text == text)
            .unwrap()
            .id
    };
    let (first, third) = (id(cx, "first"), id(cx, "third"));
    for (moved, before) in [(third, Some(first)), (first, None)] {
        state
            .command(
                cx,
                Command::MoveQueued {
                    session_id: "thread".into(),
                    id: moved,
                    before,
                },
            )
            .unwrap();
    }
    let mut sent = Vec::new();
    for turn in 0..3 {
        provider.play([completed_turn(&format!("turn-{turn}"))]);
        let (delivery_id, text) = provider.next_turn(cx);
        sent.push(text);
        provider.play([
            AgentEvent::TurnAccepted { delivery_id },
            AgentEvent::TurnStarted {
                turn_id: format!("turn-{}", turn + 1),
            },
        ]);
    }
    assert_eq!(sent, ["third", "second", "first"]);
}

/// A message scheduled for later lets an ordinary one sent after it go
/// first, and stays scheduled once that one is taken.
#[test]
fn future_scheduled_head_does_not_block_ordinary_dispatch_or_acceptance() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scheduled-head-test");
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.open(cx, "thread");
    state
        .command(
            cx,
            Command::ScheduleTurn {
                session_id: "thread".into(),
                text: "later".into(),
                attachment_paths: Vec::new(),
                due: tcode_protocol::TurnDue::AfterSecs(3_600),
            },
        )
        .unwrap();
    state.send(cx, "thread", "now");
    let provider = providers.started(cx, 0);
    let (delivery_id, text) = provider.next_turn(cx);
    assert_eq!(text, "now");
    provider.play([AgentEvent::TurnAccepted { delivery_id }]);
    cx.run_until_seen(|view| view.status("thread").delivery_in_flight.is_none());
    let queued = &cx.seen().status("thread").queued_messages;
    assert_eq!(
        queued
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        ["later"]
    );
    assert!(queued[0].fire_at_unix_secs.is_some());
}

/// A scheduled message shows when it is due and can be edited; steering it
/// sends it now, no longer scheduled or editable; dropping one removes it.
#[test]
fn schedule_status_and_queue_actions_preserve_or_remove_deadlines() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scheduled-status-test");
    stored_thread(
        &test_store,
        "scheduled-active",
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    let provider = first_turn(&state, cx, &providers, "scheduled-active", 0);
    provider.play([completed_turn("turn")]);
    cx.run_until_seen(|view| view.status("scheduled-active").running_turn.is_none());
    let schedule = |cx: &mut TestAppContext, text: &str, fire_at: u64| {
        state
            .command(
                cx,
                Command::ScheduleTurn {
                    session_id: "scheduled-active".into(),
                    text: text.into(),
                    attachment_paths: Vec::new(),
                    due: tcode_protocol::TurnDue::AtUnixSecs(fire_at),
                },
            )
            .unwrap();
    };
    let status = |cx: &TestAppContext| cx.seen().status("scheduled-active").clone();
    let fire_at = cx.now_secs() + 3_600;
    schedule(cx, "scheduled", fire_at);
    let queued = status(cx).queued_messages;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].fire_at_unix_secs, Some(fire_at));
    assert!(queued[0].editable);
    let scheduled_id = queued[0].id;

    state
        .command(
            cx,
            Command::SteerQueued {
                session_id: "scheduled-active".into(),
                id: scheduled_id,
            },
        )
        .unwrap();
    let (delivery_id, text) = provider.next_turn(cx);
    assert_eq!(text, "scheduled");
    let status_now = status(cx);
    assert_eq!(status_now.queued_messages.len(), 1);
    assert_eq!(status_now.queued_messages[0].id, delivery_id);
    assert_eq!(status_now.queued_messages[0].fire_at_unix_secs, None);
    assert!(!status_now.queued_messages[0].editable);
    assert_eq!(status_now.delivery_in_flight, Some(delivery_id));

    provider.play([
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::TurnStarted {
            turn_id: "scheduled".into(),
        },
        completed_turn("scheduled"),
    ]);
    cx.run_until_seen(|view| has_record(view, "scheduled-active", |event| {
        matches!(event, AgentEvent::TurnCompleted { turn_id, .. } if turn_id == "scheduled")
    }));
    schedule(cx, "drop me", cx.now_secs() + 7_200);
    let drop_id = status(cx).queued_messages[0].id;
    state
        .command(
            cx,
            Command::DropQueued {
                session_id: "scheduled-active".into(),
                id: drop_id,
            },
        )
        .unwrap();
    assert!(status(cx).queued_messages.is_empty());
}

/// While the harness retries a failed turn itself, what the user queued
/// waits rather than racing the retry; stopping the retry releases it. With
/// a retry the user starts, the next message goes as usual, and a start the
/// harness refuses clears the offer.
#[test]
fn native_retry_keeps_queue_waiting_and_cancellation_releases_it() {
    for automatic in [true, false] {
        let cx = &mut TestAppContext::default();
        let test_store = TestStore::new("native-retry-queue");
        stored_thread(
            &test_store,
            "retry",
            ProviderKind::Codex,
            &test_store.root().join("retry"),
            |_| {},
        );
        let (state, providers) = played_host(cx, &test_store);
        state.open(cx, "retry");
        state.send(cx, "retry", "Go");
        let provider = providers.started(cx, 0);
        provider.start_turn(cx, "failed");
        if automatic {
            state.send(cx, "retry", "queued user message");
        }
        let retry = agent::TurnRetry {
            turn_id: "failed".into(),
            retry_at: automatic.then(|| cx.now_secs() + 10),
        };
        provider.play([
            AgentEvent::TurnRetry {
                retry: Some(retry.clone()),
            },
            completed_turn("failed"),
        ]);
        cx.run_until_seen(|view| has_record(view, "retry", is_turn_end));
        let status = cx.seen().status("retry");
        assert_eq!(status.activity.working, automatic);
        assert!(!status.activity.turn_running);
        assert_eq!(status.turn_retry, Some(retry));
        assert!(
            next_command(&provider.commands).is_err(),
            "queued text must not race the native retry"
        );
        if automatic {
            state
                .command(
                    cx,
                    Command::Interrupt {
                        session_id: "retry".into(),
                    },
                )
                .unwrap();
            assert!(matches!(
                provider.next_command(cx),
                SessionCommand::Interrupt
            ));
            provider.play([AgentEvent::TurnRetry { retry: None }]);
        } else {
            state.send(cx, "retry", "queued user message");
        }
        let (delivery_id, text) = provider.next_turn(cx);
        assert_eq!(text, "queued user message");
        // A rejected new start has no TurnStarted to clear the previous offer.
        provider.play([
            AgentEvent::TurnAccepted { delivery_id },
            turn_completed("", TurnStatus::Failed),
        ]);
        cx.run_until_seen(|view| {
            view.records("retry")
                .iter()
                .filter(|record| is_turn_end(&record.event))
                .count()
                == 2
        });
        let status = cx.seen().status("retry");
        assert!(status.turn_retry.is_none());
        assert!(!status.activity.working);
        assert!(
            state
                .command(
                    cx,
                    Command::RetryTurn {
                        session_id: "retry".into(),
                        turn_id: "failed".into(),
                    },
                )
                .is_err()
        );
    }
}

/// A usage limit the harness reports queues the resume prompt for when the
/// limit resets, unless the user turned that off.
#[test]
fn usage_limit_event_schedules_resume_when_enabled_only() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-usage-limit-resume-test");
    for id in ["resume-enabled", "resume-disabled"] {
        stored_thread(
            &test_store,
            id,
            ProviderKind::ClaudeCode,
            &test_store.root().join(id),
            |_| {},
        );
    }
    let (state, providers) = played_host(cx, &test_store);
    let resets_at = cx.now_secs() + 3_600;
    for (n, id) in ["resume-enabled", "resume-disabled"]
        .into_iter()
        .enumerate()
    {
        if n == 1 {
            state
                .command(
                    cx,
                    Command::PatchSettings {
                        patch: SettingsPatch::ResumeOnLimitReset(false),
                    },
                )
                .unwrap();
        }
        let provider = first_turn(&state, cx, &providers, id, n);
        provider.play([AgentEvent::UsageLimitReached { resets_at }]);
        provider.fence(cx, id);
    }
    let queued = &cx.seen().status("resume-enabled").queued_messages;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].text, tcode_core::session::RESUME_PROMPT);
    assert_eq!(queued[0].fire_at_unix_secs, Some(resets_at));
    assert!(
        cx.seen()
            .status("resume-disabled")
            .queued_messages
            .is_empty()
    );
}

/// A scheduled message goes out as an ordinary message when it comes due.
#[test]
fn due_scheduled_message_reenters_the_ordinary_send_path() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scheduled-fire-test");
    stored_thread(
        &test_store,
        "due-active",
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.open(cx, "due-active");
    state
        .command(
            cx,
            Command::ScheduleTurn {
                session_id: "due-active".into(),
                text: "due now".into(),
                attachment_paths: Vec::new(),
                due: tcode_protocol::TurnDue::AfterSecs(60),
            },
        )
        .unwrap();
    cx.clock.advance(Duration::from_secs(60));
    assert_eq!(providers.started(cx, 0).next_turn(cx).1, "due now");
}

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

const HANDED: &str = "handed-over";

fn native(session: &str) -> agent::ResumeCursor {
    agent::ResumeCursor(serde_json::json!({ "session_id": session }))
}

/// The option the thread had chosen while it ran in Codex.
fn codex_effort() -> agent::OptionSelection {
    agent::OptionSelection {
        id: "effort".into(),
        value: "high".into(),
    }
}

/// A thread that ran two turns in Codex with that option and was handed to Claude Code, which
/// ran a third: each harness reported its session and a restore point per
/// prompt, as it does.
fn handed_over(cx: &mut TestAppContext, test_store: &TestStore) -> (TestEntity, Providers) {
    stored_thread(
        test_store,
        HANDED,
        ProviderKind::Codex,
        &test_store.root().join("thread"),
        |meta| {
            meta.model = Some("gpt-5".into());
            meta.option_selections = vec![codex_effort()];
        },
    );
    let (state, providers) = played_host(cx, test_store);
    state.open(cx, HANDED);
    let turn = |cx: &mut TestAppContext, played: &Played, name: &str| {
        played.start_turn(cx, &format!("{name}-turn"));
        played.play([
            AgentEvent::TurnCheckpoint {
                turn_id: format!("{name}-turn"),
                checkpoint_id: name.into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("{name}-answer"),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: format!("answer in {name}"),
                    summarized: false,
                },
            }),
            completed_turn(&format!("{name}-turn")),
        ]);
        played.fence(cx, HANDED);
    };
    let started = |played: &Played, session: &str| {
        played.play([AgentEvent::SessionStarted {
            provider_session_id: session.into(),
            resume: native(session),
            model: None,
        }]);
    };
    state.send(cx, HANDED, "first");
    let codex = providers.started(cx, 0);
    started(&codex, "codex-native");
    turn(cx, &codex, "codex-1");
    state.send(cx, HANDED, "second");
    turn(cx, &codex, "codex-2");
    state
        .command(
            cx,
            Command::SetActiveModel {
                session_id: HANDED.into(),
                provider: ProviderKind::ClaudeCode,
                model: Some("claude-opus-5-5".into()),
                profile_id: None,
            },
        )
        .unwrap();
    state
        .command(
            cx,
            Command::ConfirmRelayAndSend {
                session_id: HANDED.into(),
                text: "third".into(),
                attachment_paths: Vec::new(),
            },
        )
        .unwrap();
    let claude = providers.started(cx, 1);
    assert_eq!(claude.provider, ProviderKind::ClaudeCode);
    started(&claude, "claude-native");
    turn(cx, &claude, "claude-1");
    (state, providers)
}

fn rewind_handed(state: &TestEntity, cx: &mut TestAppContext, turn: usize) {
    let command = serde_json::from_value(serde_json::json!({
        "type": "rewind_turn",
        "content": {
            "session_id": HANDED, "turn": turn, "mode": "conversation", "client_id": "desk",
        }
    }))
    .unwrap();
    state.command(cx, command).unwrap();
}

/// The harness and model the thread is on, as its status says.
fn backend(cx: &TestAppContext) -> (ProviderKind, Option<String>) {
    let status = cx.seen().status(HANDED);
    (status.provider, status.requested_model.clone())
}

fn has_relay(cx: &TestAppContext) -> bool {
    cx.seen()
        .timeline(HANDED)
        .entries
        .iter()
        .any(|entry| matches!(entry.content, EntryContent::ProviderRelay { .. }))
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
        host.set_provider_launcher(launcher);
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

/// A Claude Code thread whose harness runs `live` and whose turn has ended,
/// with `background` pieces of work going on, the user then choosing
/// `chosen` for its next message.
fn settings_scene(
    cx: &mut TestAppContext,
    store: &SessionStore,
    id: &str,
    background: usize,
    chosen: &str,
) -> (TestEntity, Providers, Played) {
    stored_thread(
        store,
        id,
        ProviderKind::ClaudeCode,
        &store.root().join(id),
        |meta| {
            meta.model = Some("claude-opus-4-8".into());
        },
    );
    let (state, providers) = played_host(cx, store);
    let provider = first_turn(&state, cx, &providers, id, 0);
    provider.play([
        AgentEvent::BackgroundTasksChanged { count: background },
        completed_turn("turn"),
    ]);
    cx.run_until_seen(|view| view.status(id).running_turn.is_none());
    state
        .command(
            cx,
            Command::SetActiveModel {
                session_id: id.into(),
                provider: ProviderKind::ClaudeCode,
                model: Some(chosen.into()),
                profile_id: None,
            },
        )
        .unwrap();
    (state, providers, provider)
}

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

/// Deleting a project kills the PTY its unsent draft left running off screen
/// instead of leaving it behind forever.
#[test]
fn deleting_a_project_ends_the_terminal_its_draft_left_running() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-delete-project-draft");
    let root = test_store.root().clone();
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    state.update(cx, |state, cx| {
        state.start_draft("doomed".into(), root.clone(), cx);
    });
    let draft_id = state.read(|state| state.selected.clone().expect("draft selected"));
    open_terminal_drawer(&state, cx, 1, &draft_id);
    let terminals: Vec<_> = state.read(|state| {
        state
            .session_status_snapshot(&draft_id)
            .unwrap()
            .terminals
            .iter()
            .map(|terminal| terminal.id)
            .collect()
    });
    assert!(!terminals.is_empty());
    state.update(cx, |state, cx| state.host.park_active(&draft_id, cx));
    state.read(|state| {
        for &terminal in &terminals {
            assert!(!state.terminal_handle(terminal).unwrap().exited());
        }
    });

    state.update(cx, |state, cx| {
        state.host.delete_project("doomed", cx).detach()
    });
    cx.run_until(|state| {
        terminals.iter().all(|&terminal| {
            state
                .terminal_handle(terminal)
                .is_none_or(|handle| handle.exited())
        })
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
}

#[test]
fn a_drafts_terminal_starts_in_its_project_directory() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-terminal-open");
    let project_cwd = test_store.root().join("project");
    std::fs::create_dir_all(&project_cwd).unwrap();
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    state.update(cx, |state, cx| {
        state.start_draft("terminal-project".into(), project_cwd.clone(), cx);
    });
    let draft_id = state.read(|state| state.selected.clone().expect("draft selected"));
    open_terminal_drawer(&state, cx, 1, &draft_id);

    state.read(|state| {
        let workspace = &state.resident(&draft_id).unwrap().terminal_workspace;
        assert_eq!(workspace.terminals.len(), 1);
        assert!(
            state
                .session_status_snapshot(&draft_id)
                .unwrap()
                .terminal_open
        );
        assert_eq!(
            std::fs::canonicalize(workspace.terminals[0].terminal.working_directory()).unwrap(),
            std::fs::canonicalize(&project_cwd).unwrap()
        );
    });
}

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

/// A repository at `root` on `main` with one commit of `tracked.txt`.
fn init_git_repo(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    run_git(root, &["init", "-b", "main"]).unwrap();
    let config = root.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(
        "[commit]\n\tgpgsign = false\n\
         [core]\n\thooksPath = .no-hooks\n\tautocrlf = false\n\
         [user]\n\tname = tcode\n\temail = tcode@localhost\n",
    );
    std::fs::write(&config, text).unwrap();
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
        state.set_provider_launcher(launcher);
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
    let project = Project::from_root(root.clone());
    test_store.upsert_project(&project).unwrap();
    stored_thread(
        &test_store,
        "parent",
        ProviderKind::Codex,
        &root,
        |parent| {
            parent.project_id = Some(project.id.clone());
        },
    );
    let (state, providers) = controlled_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let lead = first_turn(&state, cx, &providers, "parent", 0);
    let response = lead
        .control(
            cx,
            "start_thread",
            serde_json::json!({
                "workspace": "worktree",
                "subagent": true,
                "provider": "codex",
                "model": "gpt-6.1-sol",
                "title": "Isolated child",
                "message": "Inspect the workspace",
                "request_id": "isolated",
            }),
        )
        .unwrap();

    let child_id = response["thread_id"].as_str().unwrap().to_string();
    let expected_branch = format!("tcode/{child_id}");
    let expected_path = PathBuf::from(response["cwd"].as_str().unwrap());
    assert!(
        expected_path.starts_with(&data_worktrees) && expected_path.ends_with(&child_id),
        "start_thread response: {response}"
    );
    assert_eq!(response["worktree_branch"], expected_branch);
    cx.run_until_seen(|view| view.session(&child_id).is_some());
    {
        let child = cx.seen().session(&child_id).unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(child.cwd, expected_path);
        assert_eq!(
            child
                .worktree
                .as_ref()
                .map(|worktree| worktree.branch.as_str()),
            Some(expected_branch.as_str())
        );
    }
    assert_eq!(providers.started(cx, 1).options.cwd, expected_path);
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

    let summary = cleanup_orphans(&store, std::time::SystemTime::now()).unwrap();

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
        .map(|(ts, message)| StoredEvent {
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
        let records: Vec<StoredEvent> = (0..2000)
            .map(|index| StoredEvent {
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

/// Persist `turns` streamed turns for `id` and return the records as the
/// store replays them.
fn persist_streamed_turns(store: &SessionStore, id: &str, turns: u64) -> Vec<StoredEvent> {
    for turn in 0..turns {
        let mut events = vec![
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("user-{turn}"),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: format!("question {turn}"),
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

fn tool_call(id: &str, output: String) -> StoredEvent {
    StoredEvent {
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
                family: None,
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

/// A stored turn-changes snapshot: the whole diff of `turn` so far.
fn stored_snapshot(ts: u64, turn: &str, diff: &str) -> String {
    format!(
        "{{\"ts\":{ts},\"event\":{{\"type\":\"turn_changes_updated\",\"turn_id\":\"{turn}\",\
         \"changes\":[{{\"path\":\"f\",\"kind\":\"modify\",\"diff\":{}}}],\
         \"completeness\":\"exact\"}}}}\n",
        serde_json::to_string(&Some(diff).filter(|diff| !diff.is_empty())).unwrap()
    )
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

/// A log as a read of densely numbered rows would load it.
fn session_log(records: impl IntoIterator<Item = StoredEvent>) -> SessionLog {
    let records: Vec<_> = records.into_iter().collect();
    let rows = (0..records.len() as u64).collect();
    SessionLog::new(tcode_services::store::EventLog {
        next_row: records.len() as u64,
        records,
        rows,
    })
}

/// The reply a subscription to a session whose log is resident gets.
fn events_reply(state: &AppState, subscription: &Subscription) -> EventEnvelope {
    let Topic::SessionEvents { session_id } = &subscription.topic else {
        panic!("a subscription to a session's events")
    };
    EventEnvelope {
        request_id: None,
        topic: subscription.topic.clone(),
        event: state.event_records[session_id].events_window(subscription, None),
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
    records: Vec<StoredEvent>,
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

fn next_host_event(events: &async_channel::Receiver<EventEnvelope>) -> EventEnvelope {
    events
        .recv_blocking()
        .expect("the host publishes the lifecycle change before it stops")
}

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
            assert!(meta.updated_at > stamp / 1000);
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
fn manual_settlement_is_thread_local_cancels_automatic_queue_and_blocks_human_admission() {
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

    // A message the user queues for later keeps the thread out.
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
    assert_eq!(
        cx.seen().status("parent").queued_messages[0].text,
        tcode_core::session::RESUME_PROMPT
    );
    parent.play([AgentEvent::UserInputRequested {
        request_id: "asynchronous".into(),
        questions: vec![],
        delivery: agent::UserInputDelivery::Async,
    }]);
    parent.fence(cx, "parent");
    settle(cx).unwrap();
    assert!(matches!(parent.next_command(cx), SessionCommand::Shutdown));
    let settled = |cx: &TestAppContext, id: &str| cx.seen().session(id).unwrap().is_settled();
    assert!(settled(cx, "parent"));
    assert!(cx.seen().session("parent").unwrap().unsettled_at.is_none());
    assert!(
        cx.seen().status("parent").queued_messages.is_empty(),
        "the automatic queue goes with the settle"
    );
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
            state.handle_control_op("lead".into(), op, reply, cx)
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
        settings_store.profile_secrets("github").unwrap()["ANTHROPIC_API_KEY"],
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
            .unwrap()
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
        settings_store.profile_secrets("github").unwrap()["ANTHROPIC_API_KEY"],
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
    cx.run_until(|_| settings_store.profile_secrets("github").unwrap().is_empty());
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
    let work = [
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        AgentEvent::ItemStarted(ThreadItem {
            id: "build".into(),
            parent_item_id: None,
            content: ItemContent::CommandExecution {
                command: "make".into(),
                input: Vec::new(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::InProgress,
            },
        }),
    ];
    // The host stopped an hour before the thread was opened again.
    let stopped = test_clock().host_clock().unix_millis();
    test_store.upsert_meta(&meta).unwrap();
    if restarted {
        // A host whose turn was running when it stopped.
        let cx = &mut TestAppContext::default();
        let (state, providers) = played_host(cx, &test_store);
        state.open(cx, "thread");
        state.send(cx, "thread", "Do the task");
        let provider = providers.started(cx, 0);
        provider.start_turn(cx, "turn-1");
        provider.play([work[1].clone()]);
        provider.fence(cx, "thread");
        assert_eq!(cx.now_millis(), stopped);
    }
    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let commands = scripted.commands.clone();
    let mut cx = TestAppContext::default();
    cx.clock.advance(Duration::from_secs(3_600));
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher(scripted.launcher);
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

#[test]
fn a_disabled_leads_child_report_waits_instead_of_steering() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("disabled-lead-report");
    let fleet = Fleet::new(cx, &store, ProviderKind::Codex, &["child"], |_| {});
    let toggle = |enabled| tcode_protocol::Command::UpdateProfileSettings {
        profile_id: Settings::builtin_profile_id(ProviderKind::Codex).into(),
        patch: ProfileSettingsPatch::SetEnabled { enabled },
    };
    fleet.state.command(cx, toggle(false)).unwrap();
    fleet.child("child").play([
        report_result_event("report", "retained report"),
        completed_turn("turn-1"),
    ]);
    fleet.child("child").fence(cx, "child");
    let queued = &cx.seen().status("parent").queued_messages;
    assert_eq!(queued.len(), 1);
    assert!(queued[0].text.ends_with("retained report"));
    assert!(next_command(&fleet.lead.commands).is_err());
    fleet.lead.play([completed_turn("turn")]);
    fleet.lead.fence(cx, "parent");
    fleet.state.command(cx, toggle(true)).unwrap();
    assert!(fleet.lead.next_turn(cx).1.ends_with("retained report"));
}
