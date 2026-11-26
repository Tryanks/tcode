use super::*;
use agent::{ApprovalDecision, ApprovalKind, FileChangeKind, RewindMode};
use serde_json::json;

/// A thread's latest activity is what it last started: the stream of a
/// running item leaves it be, an item only ever seen through its deltas
/// counts from the first, a child's items are its mirror's, and the end
/// of the turn clears it for good, whatever straggles in after.
#[test]
fn latest_activity_follows_item_starts_and_clears_when_the_turn_ends() {
    let item = |id: &str, content| ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content,
    };
    let command = |status, output: &str| ItemContent::CommandExecution {
        command: "cargo   test\n  --locked".into(),
        input: Vec::new(),
        output: output.into(),
        exit_code: None,
        status,
    };
    let delta = |kind, text: &str| AgentEvent::Delta {
        item_id: match kind {
            DeltaKind::AssistantText => "text".into(),
            DeltaKind::ReasoningText => "thought".into(),
            DeltaKind::CommandOutput => "cmd".into(),
        },
        kind,
        text: text.into(),
    };
    let running_command = LatestActivity::Command {
        text: "cargo test --locked".into(),
    };
    let tool = LatestActivity::Tool {
        name: "mcp__tcode_control__read".into(),
        target: Some("src/lib.rs".into()),
    };
    let subagent = LatestActivity::Subagent {
        description: "Audit the fold".into(),
    };
    let steps = [
        (
            AgentEvent::TurnStarted {
                turn_id: "turn".into(),
            },
            None,
        ),
        (
            AgentEvent::ItemStarted(item(
                "thought",
                ItemContent::Reasoning {
                    text: String::new(),
                },
            )),
            Some(LatestActivity::Thinking),
        ),
        (
            delta(DeltaKind::ReasoningText, "maybe"),
            Some(LatestActivity::Thinking),
        ),
        (
            delta(DeltaKind::AssistantText, "Run"),
            Some(LatestActivity::Writing),
        ),
        (
            delta(DeltaKind::AssistantText, "ning"),
            Some(LatestActivity::Writing),
        ),
        (
            AgentEvent::ItemStarted(item("cmd", command(ItemStatus::InProgress, ""))),
            Some(running_command.clone()),
        ),
        (
            delta(DeltaKind::CommandOutput, "compiling\n"),
            Some(running_command.clone()),
        ),
        (
            AgentEvent::ItemCompleted(item("cmd", command(ItemStatus::Completed, "ok"))),
            Some(running_command.clone()),
        ),
        (
            AgentEvent::ItemStarted(item(
                "call",
                ItemContent::ToolCall {
                    image_reads: Vec::new(),
                    name: "mcp__tcode_control__read".into(),
                    input: json!({ "path": "src/lib.rs", "limit": 10 }),
                    output: None,
                    status: ItemStatus::InProgress,
                    family: None,
                },
            )),
            Some(tool.clone()),
        ),
        (
            AgentEvent::ItemStarted(ThreadItem {
                id: "child".into(),
                parent_item_id: Some("spawn".into()),
                content: ItemContent::Reasoning {
                    text: String::new(),
                },
            }),
            Some(tool),
        ),
        (
            AgentEvent::ItemStarted(item(
                "spawn",
                ItemContent::Subagent {
                    agent_type: "explorer".into(),
                    description: "Audit the fold".into(),
                    status: ItemStatus::InProgress,
                    summary: None,
                    model: None,
                    effort: None,
                },
            )),
            Some(subagent),
        ),
        (
            AgentEvent::TurnCompleted {
                turn_id: "turn".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            None,
        ),
        (
            AgentEvent::ItemCompleted(item("late", command(ItemStatus::Completed, ""))),
            None,
        ),
    ];
    let mut timeline = Timeline::default();
    for (index, (event, expected)) in steps.into_iter().enumerate() {
        timeline.apply_at(Some(index as u64), &event);
        assert_eq!(timeline.latest_activity(), expected, "after {event:?}");
    }
}

/// An item that starts before it can say what it does names it once it
/// can, and whatever it names is clipped for the index.
#[test]
fn latest_activity_takes_what_an_update_names_and_clips_it() {
    let call = |input| {
        AgentEvent::ItemUpdated(ThreadItem {
            id: "call".into(),
            parent_item_id: None,
            content: ItemContent::ToolCall {
                image_reads: Vec::new(),
                name: "bash".into(),
                input,
                output: None,
                status: ItemStatus::InProgress,
                family: None,
            },
        })
    };
    let mut timeline = Timeline::default();
    for event in [
        AgentEvent::TurnStarted {
            turn_id: "turn".into(),
        },
        call(serde_json::Value::Null),
    ] {
        timeline.apply_at(None, &event);
    }
    assert_eq!(
        timeline.latest_activity(),
        Some(LatestActivity::Tool {
            name: "bash".into(),
            target: None,
        })
    );
    timeline.apply_at(None, &call(json!({ "command": "é".repeat(500) })));
    assert_eq!(
        timeline.latest_activity(),
        Some(LatestActivity::Tool {
            name: "bash".into(),
            target: Some("é".repeat(160)),
        })
    );
}

/// A tool window says how long its call took: the time between the
/// item's start and its completion, and nothing for an item that only
/// ever arrived complete (an import, a provider that reports results
/// alone).
#[test]
fn an_item_seen_starting_keeps_when_it_finished() {
    let read = |id: &str, status| ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: ItemContent::ToolCall {
            image_reads: Vec::new(),
            name: "Read".into(),
            input: json!({ "file_path": "src/lib.rs" }),
            output: (status == ItemStatus::Completed).then(|| "1\tfn main() {}".into()),
            status,
            family: None,
        },
    };
    let timeline = Timeline::fold_events([
        StoredEvent::from(AgentEvent::TurnStarted {
            turn_id: "turn".into(),
        }),
        StoredEvent {
            ts: Some(1_000),
            ..AgentEvent::ItemStarted(read("seen", ItemStatus::InProgress)).into()
        },
        StoredEvent {
            ts: Some(3_400),
            ..AgentEvent::ItemCompleted(read("seen", ItemStatus::Completed)).into()
        },
        StoredEvent {
            ts: Some(5_000),
            ..AgentEvent::ItemCompleted(read("whole", ItemStatus::Completed)).into()
        },
    ]);
    let times = timeline
        .entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry.ts, entry.finished_ts))
        .collect::<Vec<_>>();
    assert_eq!(
        times,
        [
            ("seen", Some(1_000), Some(3_400)),
            ("whole", Some(5_000), None)
        ]
    );
}

#[test]
fn a_plan_streams_into_one_entry_and_feedback_on_it_reads_as_the_users_words() {
    let mut timeline = Timeline::default();
    let plan = |markdown: &str| ThreadItem {
        id: "plan-1".into(),
        parent_item_id: None,
        content: ItemContent::Plan {
            markdown: markdown.into(),
        },
    };
    for event in [
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        AgentEvent::ItemStarted(plan("")),
        AgentEvent::Delta {
            item_id: "plan-1".into(),
            kind: DeltaKind::AssistantText,
            text: "# Plan".into(),
        },
        AgentEvent::ItemCompleted(plan("# Plan\n\n1. Step")),
        AgentEvent::ApprovalRequested(ApprovalRequest {
            id: "decide".into(),
            parent_item_id: None,
            turn_id: Some("turn-1".into()),
            kind: ApprovalKind::Plan {
                item_id: "plan-1".into(),
                markdown: "# Plan\n\n1. Step".into(),
            },
            reason: None,
            blocked_path: None,
            options: Vec::new(),
        }),
    ] {
        timeline.apply_at(None, &event);
    }
    assert_eq!(timeline.pending_approvals.len(), 1);
    timeline.apply_at(
        Some(5),
        &AgentEvent::ApprovalResolved {
            request_id: "decide".into(),
            decision: ApprovalDecision::Feedback("Split step one".into()),
        },
    );
    assert!(timeline.pending_approvals.is_empty());
    let contents = timeline
        .entries
        .iter()
        .map(|entry| (&entry.content, entry.turn))
        .collect::<Vec<_>>();
    assert!(
        matches!(contents.as_slice(), [
                (EntryContent::Item(ItemContent::Plan { markdown }), 0),
                (EntryContent::Feedback { text }, 0),
            ] if markdown == "# Plan\n\n1. Step" && text == "Split step one"),
        "{contents:?}"
    );
}

fn user_msg(id: &str, text: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: ItemContent::UserMessage {
            text: text.into(),
            attachments: Vec::new(),
        },
    })
}

#[test]
fn usage_replay_without_timestamps_keeps_turn_totals_and_known_capacity() {
    let usage = |processed| TokenUsage {
        freshness: agent::ContextFreshness::Current,
        used_tokens: Some(550),
        turn_processed_tokens: processed,
        ..Default::default()
    };
    let timeline = Timeline::fold_events([
        AgentEvent::TurnStarted {
            turn_id: "one".into(),
        },
        AgentEvent::TokenUsage(TokenUsage {
            context_window: Some(1000000),
            ..usage(None)
        }),
        AgentEvent::TurnCompleted {
            turn_id: "one".into(),
            status: TurnStatus::Completed,
            usage: Some(usage(Some(100))),
        },
        AgentEvent::TurnStarted {
            turn_id: "two".into(),
        },
        AgentEvent::TokenUsage(usage(None)),
        AgentEvent::TurnCompleted {
            turn_id: "two".into(),
            status: TurnStatus::Interrupted,
            usage: Some(usage(Some(20))),
        },
    ]);
    let usage = timeline.usage.unwrap();
    assert_eq!(usage.total_processed_tokens, Some(120));
    assert_eq!(usage.context_window, Some(1000000));
    assert_eq!(timeline.turns.len(), 2);
}

#[test]
fn provider_relay_marker_folds_before_the_next_user_message() {
    let timeline = Timeline::fold_events([
        user_msg("u1", "before"),
        AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: TurnStatus::Completed,
            usage: None,
        },
        AgentEvent::ProviderRelay {
            from_provider: agent::ProviderKind::ClaudeCode,
            from_model: Some("opus".into()),
            to_provider: agent::ProviderKind::Codex,
            to_model: Some("gpt-5".into()),
            prompt: None,
            moved_from: None,
            left: None,
        },
        user_msg("u2", "after"),
    ]);

    assert_eq!(timeline.turns.len(), 2);
    assert!(matches!(
        timeline.entries[1].content,
        EntryContent::ProviderRelay {
            from_provider: agent::ProviderKind::ClaudeCode,
            to_provider: agent::ProviderKind::Codex,
            ..
        }
    ));
    assert_eq!(timeline.entries[1].turn, timeline.entries[2].turn);
    assert!(matches!(
        &timeline.entries[2].content,
        EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "after"
    ));
}

#[test]
fn repeated_compacting_status_announces_one_compaction() {
    // Claude re-sends `status: compacting` per compaction phase and as a
    // 30s keepalive while one compaction runs.
    let compacting = || {
        AgentEvent::ContextCompacted(agent::Compaction {
            in_progress: true,
            ..Default::default()
        })
    };
    let compacted = || {
        AgentEvent::ContextCompacted(agent::Compaction {
            trigger: Some("auto".into()),
            ..Default::default()
        })
    };
    let timeline = Timeline::fold_events([
        user_msg("u1", "go"),
        compacting(),
        compacting(),
        compacting(),
        compacted(),
        compacting(),
        compacting(),
        compacted(),
    ]);

    let compactions: Vec<bool> = timeline
        .entries
        .iter()
        .filter_map(|entry| match &entry.content {
            EntryContent::ContextCompacted(c) => Some(c.in_progress),
            _ => None,
        })
        .collect();
    assert_eq!(compactions, [true, false, true, false]);
}

#[test]
fn structured_user_input_blocks_until_resolution() {
    let request = AgentEvent::UserInputRequested {
        request_id: "que_1".into(),
        parent_item_id: None,
        questions: vec![UserInputQuestion {
            id: "que_1:0".into(),
            header: "Scope".into(),
            question: "Which crate?".into(),
            options: Vec::new(),
            multi_select: false,
            prefill: None,
            optional: false,
        }],
        delivery: UserInputDelivery::Blocking,
    };
    let mut timeline = Timeline::default();
    timeline.apply_at(None, &request);
    assert_eq!(
        timeline
            .pending_user_input
            .as_ref()
            .map(|pending| pending.request_id.as_str()),
        Some("que_1")
    );

    timeline.apply_at(
        None,
        &AgentEvent::UserInputResolved {
            request_id: "que_1".into(),
            answers: serde_json::Map::new(),
        },
    );
    assert!(timeline.pending_user_input.is_none());
}

fn checkpointed_turns(count: usize) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    for index in 1..=count {
        events.extend([
            user_msg(&format!("user-{index}"), &format!("prompt {index}")),
            AgentEvent::TurnStarted {
                turn_id: format!("turn-{index}"),
            },
            AgentEvent::TurnCheckpoint {
                turn_id: format!("turn-{index}"),
                checkpoint_id: format!("checkpoint-{index}"),
            },
            AgentEvent::TurnCompleted {
                turn_id: format!("turn-{index}"),
                status: TurnStatus::Completed,
                usage: None,
            },
        ]);
    }
    events
}

fn divider(timeline: &Timeline) -> &Rewound {
    timeline
        .entries
        .iter()
        .rev()
        .find_map(|entry| match &entry.content {
            EntryContent::Rewound(rewound) => Some(&**rewound),
            _ => None,
        })
        .expect("the rewind left a divider")
}

#[test]
fn provider_conversation_rewind_is_an_append_only_timeline_marker() {
    let mut events = checkpointed_turns(3);
    // A rewind as logs before rewind dividers recorded it.
    events.push(
            serde_json::from_str(
                r#"{"type":"rewind_completed","checkpoint_id":"checkpoint-2","mode":"conversation","prefill":"prompt 2"}"#,
            )
            .unwrap(),
        );

    let mut timeline = Timeline::fold_events(events);
    assert_eq!(timeline.turns.len(), 2, "the kept turn and the divider's");
    assert!(matches!(
        &timeline.entries[0].content,
        EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "prompt 1"
    ));
    assert_eq!(
        timeline.turns[0].provider_checkpoint_id.as_deref(),
        Some("checkpoint-1")
    );
    let rewound = divider(&timeline);
    assert_eq!(rewound.removed_turns, 2);
    assert!(rewound.undo_token.is_none());
    assert!(matches!(
        &rewound.removed[0].content,
        EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "prompt 2"
    ));

    // New work after the divider joins its turn; removed history does not
    // reappear even though the underlying log remains append-only.
    timeline.apply_at(None, &user_msg("user-4", "replacement prompt"));
    assert_eq!(timeline.turns.len(), 2);
    assert!(matches!(
        &timeline.entries[2].content,
        EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "replacement prompt"
    ));
}

/// A thread outlives its provider processes, and a process numbers its
/// turns from scratch (Claude Code's and an ACP agent's `turn-1`, …). A
/// restore point a later process reports lands on that process's turn,
/// not on the thread's first turn of the same name, or rewinding the
/// first turn would cut at the later prompt.
#[test]
fn a_restore_point_lands_on_the_latest_turn_its_provider_named() {
    let mut events = Vec::new();
    for (prompt, checkpoint) in [("first", "uuid-first"), ("second", "uuid-second")] {
        // Each prompt reaches a freshly started provider process.
        events.extend([
            user_msg(prompt, prompt),
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            AgentEvent::TurnCheckpoint {
                turn_id: "turn-1".into(),
                checkpoint_id: checkpoint.into(),
            },
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ]);
    }
    let timeline = Timeline::fold_events(events);
    assert_eq!(
        timeline
            .turns
            .iter()
            .map(|turn| turn.provider_checkpoint_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("uuid-first"), Some("uuid-second")]
    );
}

fn undoable_rewind(checkpoint: &str) -> AgentEvent {
    AgentEvent::RewindCompleted {
        checkpoint_id: checkpoint.into(),
        mode: RewindMode::FilesAndConversation,
        prefill: None,
        prefill_attachments: Vec::new(),
        later_history: agent::LaterHistory::HiddenUntilNextSend,
        files: None,
        incomplete: None,
        undo_token: Some("revert".into()),
    }
}

#[test]
fn an_undoable_rewind_is_undone_until_a_message_is_sent() {
    let mut events = checkpointed_turns(3);
    events.push(undoable_rewind("checkpoint-2"));
    let mut timeline = Timeline::fold_events(events.clone());
    let mut boundaries = TurnBoundaries::default();
    for event in &events {
        boundaries.apply(event);
    }
    assert_eq!(divider(&timeline).undo_token.as_deref(), Some("revert"));

    let undone = AgentEvent::RewindUndone {
        checkpoint_id: "checkpoint-2".into(),
    };
    timeline.apply_at(None, &undone);
    boundaries.apply(&undone);
    let before = Timeline::fold_events(checkpointed_turns(3));
    assert_eq!(
        (&timeline.entries, &timeline.turns),
        (&before.entries, &before.turns),
        "the undo puts back exactly what the rewind removed"
    );
    assert_eq!(boundaries.len(), 3);

    events.push(user_msg("user-4", "replacement prompt"));
    let mut timeline = Timeline::fold_events(events);
    assert!(divider(&timeline).undo_token.is_none());
    timeline.apply_at(None, &undone);
    assert_eq!(
        timeline.turns.len(),
        2,
        "a sent message made the rewind final"
    );
}

#[test]
fn a_rewind_to_a_steer_keeps_the_turn_before_it() {
    let steer = |id: &str, checkpoint: &str| {
        [
            AgentEvent::SteerRequested {
                request_id: id.into(),
                text: format!("also {id}"),
                attachments: Vec::new(),
            },
            AgentEvent::SteerAccepted {
                request_id: id.into(),
            },
            AgentEvent::SteerCheckpoint {
                request_id: id.into(),
                checkpoint_id: checkpoint.into(),
            },
        ]
    };
    let mut events = checkpointed_turns(1);
    events.truncate(3);
    events.extend(steer("steer-1", "steer-checkpoint"));
    events.push(assistant("reply", "done"));
    events.extend(steer("steer-2", "later-steer-checkpoint"));
    events.push(AgentEvent::TurnCompleted {
        turn_id: "turn-1".into(),
        status: TurnStatus::Completed,
        usage: None,
    });
    let before = Timeline::fold_events(events.clone());
    let rewind = |undo_token: Option<&str>| AgentEvent::RewindCompleted {
        checkpoint_id: "steer-checkpoint".into(),
        mode: RewindMode::Conversation,
        prefill: None,
        prefill_attachments: Vec::new(),
        later_history: agent::LaterHistory::HiddenUntilNextSend,
        files: None,
        incomplete: None,
        undo_token: undo_token.map(str::to_owned),
    };

    let mut cut = events.clone();
    cut.push(rewind(None));
    let mut timeline = Timeline::fold_events(cut);
    assert!(matches!(
        &timeline.entries[0].content,
        EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "prompt 1"
    ));
    assert_eq!(timeline.entries[0].turn, 0);
    let rewound = divider(&timeline);
    assert_eq!(rewound.removed_turns, 0);
    assert_eq!(
        rewound.removed.len(),
        3,
        "the steer, the reply and the steer after it"
    );
    assert!(
        timeline.turns[0].steer_checkpoints.is_empty(),
        "no restore point is left for a steer the cut removed"
    );
    assert_eq!(timeline.turns.len(), 2);
    // A client that still showed the later steer cannot cut there again.
    let mut stale = rewind(None);
    if let AgentEvent::RewindCompleted { checkpoint_id, .. } = &mut stale {
        *checkpoint_id = "later-steer-checkpoint".into();
    }
    timeline.apply_at(None, &stale);
    assert_eq!(timeline.turns.len(), 2);

    // An undo gives the turn back its steers' restore points.
    events.push(rewind(Some("revert")));
    let mut timeline = Timeline::fold_events(events);
    timeline.apply_at(
        None,
        &AgentEvent::RewindUndone {
            checkpoint_id: "steer-checkpoint".into(),
        },
    );
    assert_eq!(
        (&timeline.entries, &timeline.turns),
        (&before.entries, &before.turns)
    );
}

#[test]
fn partial_turn_changes_include_only_successful_top_level_file_operations() {
    for (label, parent, status, included) in [
        ("completed", None, ItemStatus::Completed, true),
        ("failed", None, ItemStatus::Failed, false),
        ("in progress", None, ItemStatus::InProgress, false),
        ("child", Some("spawn-1"), ItemStatus::Completed, false),
    ] {
        let mut timeline = Timeline::fold_events([user_msg("user-1", "edit it"), turn_started()]);
        for (id, path, diff) in [
            (
                "edit-1",
                "src/lib.rs",
                "--- a/src/lib.rs\n+++ b/src/lib.rs\n-old\n+middle\n",
            ),
            ("edit-2", "other.rs", "-gone\n+new\n"),
            ("edit-3", "src/lib.rs", "-middle\n-final\n+replacement\n"),
        ] {
            timeline.apply_at(
                None,
                &AgentEvent::ItemCompleted(ThreadItem {
                    id: id.into(),
                    parent_item_id: parent.map(str::to_string),
                    content: ItemContent::FileChange {
                        changes: vec![FileChange {
                            path: path.into(),
                            kind: FileChangeKind::Modify,
                            diff: Some(diff.into()),
                        }],
                        status,
                        output: None,
                        command: None,
                    },
                }),
            );
        }
        let changes = &timeline.turns[0].changes;
        assert_eq!(changes.is_some(), included, "{label}");
        if let Some(changes) = changes {
            assert_eq!(changes.completeness, ChangeCompleteness::Partial);
            assert_eq!(
                changes
                    .changes
                    .iter()
                    .map(|change| (change.path.as_str(), change.diff.as_deref()))
                    .collect::<Vec<_>>(),
                [
                    (
                        "src/lib.rs",
                        Some(
                            "--- a/src/lib.rs\n+++ b/src/lib.rs\n-old\n+middle\n-middle\n-final\n+replacement\n"
                        )
                    ),
                    ("other.rs", Some("-gone\n+new\n")),
                ],
                "{label}"
            );
        }
    }
}

#[test]
fn completed_file_snapshot_keeps_diff_from_started_snapshot() {
    let path = "/tmp/tcode-outside-workspace.txt";
    let timeline = Timeline::fold_events([
        user_msg("user-1", "write it"),
        AgentEvent::ItemStarted(ThreadItem {
            id: "write-external".into(),
            parent_item_id: None,
            content: ItemContent::FileChange {
                changes: vec![FileChange {
                    path: path.into(),
                    kind: FileChangeKind::Create,
                    diff: Some("+visible diff".into()),
                }],
                status: ItemStatus::InProgress,
                output: None,
                command: None,
            },
        }),
        AgentEvent::ItemCompleted(ThreadItem {
            id: "write-external".into(),
            parent_item_id: None,
            content: ItemContent::FileChange {
                changes: vec![FileChange {
                    path: path.into(),
                    kind: FileChangeKind::Create,
                    diff: None,
                }],
                status: ItemStatus::Completed,
                output: None,
                command: None,
            },
        }),
    ]);

    let change_set = timeline.turns[0]
        .changes
        .as_ref()
        .expect("completed change set");
    assert_eq!(change_set.changes.len(), 1);
    assert_eq!(change_set.changes[0].path, path);
    assert_eq!(change_set.changes[0].diff.as_deref(), Some("+visible diff"));
}

#[test]
fn exact_turn_snapshot_replaces_partial_operations_and_survives_late_items() {
    let mut timeline = Timeline::fold_events([
        user_msg("user-1", "edit it"),
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        AgentEvent::ItemCompleted(ThreadItem {
            id: "edit-1".into(),
            parent_item_id: None,
            content: ItemContent::FileChange {
                changes: vec![FileChange {
                    path: "src/lib.rs".into(),
                    kind: FileChangeKind::Modify,
                    diff: Some("-intermediate\n+value\n".into()),
                }],
                status: ItemStatus::Completed,
                output: None,
                command: None,
            },
        }),
    ]);

    timeline.apply_at(
        None,
        &AgentEvent::TurnChangesUpdated {
            turn_id: "turn-1".into(),
            changes: vec![FileChange {
                path: "src/lib.rs".into(),
                kind: FileChangeKind::Modify,
                diff: Some("-before\n+after\n".into()),
            }],
            completeness: ChangeCompleteness::Exact,
        },
    );
    timeline.apply_at(
        None,
        &AgentEvent::ItemCompleted(ThreadItem {
            id: "edit-2".into(),
            parent_item_id: None,
            content: ItemContent::FileChange {
                changes: vec![FileChange {
                    path: "late.txt".into(),
                    kind: FileChangeKind::Create,
                    diff: Some("+late\n".into()),
                }],
                status: ItemStatus::Completed,
                output: None,
                command: None,
            },
        }),
    );

    let change_set = timeline.turns[0].changes.as_ref().unwrap();
    assert_eq!(change_set.completeness, ChangeCompleteness::Exact);
    assert_eq!(change_set.changes.len(), 1);
    assert_eq!(change_set.changes[0].path, "src/lib.rs");
    assert_eq!(
        change_set.changes[0].diff.as_deref(),
        Some("-before\n+after\n")
    );
}

#[test]
fn delayed_turn_snapshot_attaches_by_provider_turn_id() {
    let mut timeline = Timeline::fold_events([
        user_msg("user-1", "first"),
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        },
        AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: TurnStatus::Completed,
            usage: None,
        },
        user_msg("user-2", "second"),
        AgentEvent::TurnStarted {
            turn_id: "turn-2".into(),
        },
    ]);
    timeline.apply_at(
        None,
        &AgentEvent::TurnChangesUpdated {
            turn_id: "turn-1".into(),
            changes: vec![FileChange {
                path: "first.txt".into(),
                kind: FileChangeKind::Create,
                diff: Some("+first\n".into()),
            }],
            completeness: ChangeCompleteness::Exact,
        },
    );

    assert_eq!(
        timeline.turns[0].changes.as_ref().unwrap().changes[0].path,
        "first.txt"
    );
    assert!(timeline.turns[1].changes.is_none());
}

#[test]
fn correlated_steering_replays_pending_then_only_matching_acceptance() {
    let request = AgentEvent::SteerRequested {
        request_id: "steer-a".into(),
        text: "change direction".into(),
        attachments: Vec::new(),
    };
    let mut timeline = Timeline::fold_events([
        user_msg("user-a", "start"),
        AgentEvent::TurnStarted {
            turn_id: "turn-a".into(),
        },
        request.clone(),
    ]);

    assert!(matches!(
        &timeline.entries[1].content,
        EntryContent::Steer {
            text,
            status: SteeringStatus::Pending,
            ..
        } if text == "change direction"
    ));

    timeline.apply_at(
        None,
        &AgentEvent::SteerAccepted {
            request_id: "steer-b".into(),
        },
    );
    assert!(matches!(
        timeline.entries[1].content,
        EntryContent::Steer {
            status: SteeringStatus::Pending,
            ..
        }
    ));

    let accepted = AgentEvent::SteerAccepted {
        request_id: "steer-a".into(),
    };
    timeline.apply_at(None, &accepted);
    assert!(matches!(
        timeline.entries[1].content,
        EntryContent::Steer {
            status: SteeringStatus::Accepted,
            ..
        }
    ));

    // A restart folds the persisted request and acceptance to the same
    // accepted state; confirmation cannot regress to pending on replay.
    let replayed = Timeline::fold_events([
        user_msg("user-a", "start"),
        AgentEvent::TurnStarted {
            turn_id: "turn-a".into(),
        },
        request,
        accepted,
    ]);
    assert!(matches!(
        replayed.entries[1].content,
        EntryContent::Steer {
            status: SteeringStatus::Accepted,
            ..
        }
    ));
}

#[test]
fn rejected_steer_leaves_the_timeline_live_and_on_replay() {
    let events = [
        user_msg("user-a", "start"),
        AgentEvent::TurnStarted {
            turn_id: "turn-a".into(),
        },
        AgentEvent::SteerRequested {
            request_id: "steer-a".into(),
            text: "late".into(),
            attachments: Vec::new(),
        },
        AgentEvent::TurnCompleted {
            turn_id: "turn-a".into(),
            status: TurnStatus::Completed,
            usage: None,
        },
        AgentEvent::SteerRejected {
            request_id: "steer-a".into(),
        },
    ];
    let mut live = Timeline::fold_events(events[..4].iter().cloned());
    assert!(live.entries.iter().any(|entry| entry.id == "steer-a"));
    live.apply_at(None, &events[4]);
    assert!(!live.entries.iter().any(|entry| entry.id == "steer-a"));

    let replayed = Timeline::fold_events(events);
    assert!(!replayed.entries.iter().any(|entry| entry.id == "steer-a"));
}

#[test]
fn accepted_steer_moves_to_its_consumption_position_live_and_on_replay() {
    let assistant_item = |id: &str| {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: id.into(),
                summarized: false,
            },
        })
    };
    let events = vec![
        user_msg("user-a", "start"),
        AgentEvent::TurnStarted {
            turn_id: "turn-a".into(),
        },
        AgentEvent::SteerRequested {
            request_id: "S".into(),
            text: "change direction".into(),
            attachments: Vec::new(),
        },
        assistant_item("A"),
        assistant_item("B"),
        AgentEvent::SteerAccepted {
            request_id: "S".into(),
        },
        assistant_item("C"),
        assistant_item("D"),
    ];

    let mut live = Timeline::default();
    for event in &events {
        live.apply_at(None, event);
    }
    let replayed = Timeline::fold_events(events);

    let live_ids: Vec<&str> = live.entries.iter().map(|entry| entry.id.as_str()).collect();
    let replayed_ids: Vec<&str> = replayed
        .entries
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(live_ids, ["user-a", "A", "B", "S", "C", "D"]);
    assert_eq!(replayed_ids, live_ids);
    assert!(matches!(
        live.entries[3].content,
        EntryContent::Steer {
            status: SteeringStatus::Accepted,
            ..
        }
    ));
    assert!(matches!(
        replayed.entries[3].content,
        EntryContent::Steer {
            status: SteeringStatus::Accepted,
            ..
        }
    ));
    assert_eq!(replayed.entries[3].turn, live.entries[3].turn);
}

fn assistant_delta(id: &str, text: &str) -> AgentEvent {
    AgentEvent::Delta {
        item_id: id.into(),
        kind: DeltaKind::AssistantText,
        text: text.into(),
    }
}

#[test]
fn streamed_item_snapshots_preserve_partial_text_and_accept_rewrites() {
    for kind in [
        DeltaKind::AssistantText,
        DeltaKind::ReasoningText,
        DeltaKind::CommandOutput,
    ] {
        let snapshot = |text: &str| {
            AgentEvent::ItemUpdated(ThreadItem {
                id: "msg".into(),
                parent_item_id: None,
                content: match kind {
                    DeltaKind::AssistantText => ItemContent::AssistantMessage {
                        text: text.into(),
                        summarized: false,
                    },
                    DeltaKind::ReasoningText => ItemContent::Reasoning { text: text.into() },
                    DeltaKind::CommandOutput => ItemContent::CommandExecution {
                        command: "echo hi".into(),
                        input: Vec::new(),
                        output: text.into(),
                        exit_code: Some(0),
                        status: ItemStatus::Completed,
                    },
                },
            })
        };
        let mut timeline = Timeline::fold_events([
            AgentEvent::Delta {
                item_id: "msg".into(),
                kind,
                text: "第一段\n".into(),
            },
            AgentEvent::Delta {
                item_id: "msg".into(),
                kind,
                text: "second".into(),
            },
        ]);
        for (incoming, expected) in [
            ("第一段\n", "第一段\nsecond"),
            ("", "第一段\nsecond"),
            ("第一段\nsecond", "第一段\nsecond"),
            ("rewritten", "rewritten"),
        ] {
            timeline.apply_at(None, &snapshot(incoming));
            assert_eq!(timeline.entries.len(), 1);
            let text = match &timeline.entries[0].content {
                EntryContent::Item(
                    ItemContent::AssistantMessage { text, .. } | ItemContent::Reasoning { text },
                ) => text,
                EntryContent::Item(ItemContent::CommandExecution {
                    command,
                    input: _,
                    output,
                    exit_code,
                    status,
                }) => {
                    assert_eq!(command, "echo hi");
                    assert_eq!(*exit_code, Some(0));
                    assert_eq!(*status, ItemStatus::Completed);
                    output
                }
                other => panic!("unexpected item: {other:?}"),
            };
            assert_eq!(text, expected, "{kind:?}: snapshot {incoming:?}");
        }
    }
}

#[test]
fn timestamps_and_turn_grouping_fold_across_two_exchanges() {
    let mut timeline = Timeline::fold_events([
        at(
            999_900,
            AgentEvent::SessionStarted {
                provider_session_id: "78b7774c".into(),
                resume: ResumeCursor(json!({ "session_id": "78b7774c" })),
                model: Some("claude-opus-4-8".into()),
            },
        ),
        at(1_000_000, user_msg("u1", "first")),
        at(
            1_000_500,
            AgentEvent::TurnStarted {
                turn_id: "t1".into(),
            },
        ),
        at(1_001_000, assistant_delta("a1", "Hi! ")),
        at(
            1_001_500,
            assistant_delta("a1", "How can I help you today?"),
        ),
        at(1_002_000, assistant("a1", "Hi! How can I help you today?")),
        at(
            1_005_500,
            AgentEvent::TurnCompleted {
                turn_id: "t1".into(),
                status: TurnStatus::Completed,
                usage: Some(TokenUsage {
                    input_tokens: Some(3355),
                    output_tokens: Some(17),
                    ..Default::default()
                }),
            },
        ),
    ]);
    assert_eq!(timeline.entries.len(), 2);
    assert!(matches!(
        &timeline.entries[0].content,
        EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "first"
    ));
    assert!(matches!(
        &timeline.entries[1].content,
        EntryContent::Item(ItemContent::AssistantMessage { text, .. }) if text == "Hi! How can I help you today?"
    ));
    assert!(!timeline.turn_running);
    assert_eq!(timeline.last_turn_status, Some(TurnStatus::Completed));
    assert_eq!(timeline.usage.as_ref().unwrap().output_tokens, Some(17));
    assert_eq!(timeline.model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(
        timeline.resume,
        Some(ResumeCursor(json!({ "session_id": "78b7774c" })))
    );
    assert_eq!(timeline.first_user_message(), Some("first"));

    timeline.apply_stored(&at(2_000_000, user_msg("u2", "second")));
    timeline.apply_stored(&at(
        2_000_400,
        AgentEvent::TurnStarted {
            turn_id: "t2".into(),
        },
    ));
    assert_eq!(timeline.turns.len(), 2);
    assert_eq!(timeline.turns[0].start_ts, Some(1_000_500));
    assert_eq!(timeline.turns[0].end_ts, Some(1_005_500));
    assert_eq!(timeline.turns[0].status, Some(TurnStatus::Completed));
    assert!(!timeline.turns[0].running);
    assert!(timeline.turns[1].running);
    assert!(timeline.turn_running);

    let u1 = &timeline.entries[0];
    assert_eq!(u1.ts, Some(1_000_000));
    assert_eq!(u1.turn, 0);
    let a1 = &timeline.entries[1];
    assert_eq!(a1.turn, 0);
    let u2 = timeline
            .entries
            .iter()
            .find(|e| matches!(&e.content, EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "second"))
            .unwrap();
    assert_eq!(u2.turn, 1);

    let mut cold = timeline.clone();
    cold.mark_idle();
    assert!(!cold.turn_running);
    assert!(cold.turns.iter().all(|t| !t.running));
}

#[test]
fn plan_updated_tracks_latest_steps() {
    use agent::PlanStepStatus;
    let mut timeline = Timeline::default();
    timeline.apply_at(
        None,
        &AgentEvent::PlanUpdated {
            turn_id: Some("t".into()),
            explanation: Some("Working".into()),
            steps: vec![
                PlanStep {
                    step: "a".into(),
                    status: PlanStepStatus::Completed,
                },
                PlanStep {
                    step: "b".into(),
                    status: PlanStepStatus::InProgress,
                },
            ],
        },
    );
    assert_eq!(
        timeline.plan_steps,
        [
            PlanStep {
                step: "a".into(),
                status: PlanStepStatus::Completed
            },
            PlanStep {
                step: "b".into(),
                status: PlanStepStatus::InProgress
            },
        ]
    );
    assert_eq!(timeline.plan_explanation.as_deref(), Some("Working"));
    for explanation in [Some("Revised"), None] {
        timeline.apply_at(
            None,
            &AgentEvent::PlanUpdated {
                turn_id: Some("t".into()),
                explanation: explanation.map(str::to_string),
                steps: vec![PlanStep {
                    step: "replacement".into(),
                    status: PlanStepStatus::Pending,
                }],
            },
        );
        assert_eq!(
            timeline.plan_steps,
            [PlanStep {
                step: "replacement".into(),
                status: PlanStepStatus::Pending
            }]
        );
        assert_eq!(timeline.plan_explanation.as_deref(), explanation);
    }
}

#[test]
fn provider_start_failure_folds_semantically() {
    let mut timeline = Timeline::default();
    timeline.apply_at(
        Some(1_234),
        &AgentEvent::ProviderStartFailed {
            error: "spawn failed".into(),
        },
    );

    let provider_error = &timeline.entries[0];
    assert_eq!(provider_error.ts, Some(1_234));
    assert!(timeline.turns.get(provider_error.turn).is_some());
    assert!(matches!(
        &provider_error.content,
        EntryContent::ProviderStartError { error } if error == "spawn failed"
    ));

    timeline.apply_at(
        Some(1_235),
        &AgentEvent::Error {
            message: "boom".into(),
            fatal: true,
        },
    );
    assert!(matches!(
        &timeline.entries[1].content,
        EntryContent::Error { message, .. } if message == "boom"
    ));
}

/// A harness's warning is shown nowhere but in its thread.
#[test]
fn a_warning_before_any_turn_is_an_entry_of_the_thread() {
    let mut timeline = Timeline::default();
    timeline.apply_at(
        Some(1_234),
        &AgentEvent::Warning {
            message: "config is deprecated".into(),
        },
    );

    let warning = &timeline.entries[0];
    assert!(timeline.turns.get(warning.turn).is_some());
    assert!(matches!(
        &warning.content,
        EntryContent::Warning { message } if message == "config is deprecated"
    ));
}

/// Attempts at one request are one entry, which says the latest; a request
/// retried later in the turn is its own.
#[test]
fn attempts_at_a_request_share_one_entry() {
    let mut timeline = Timeline::default();
    let retry = |message: &str| AgentEvent::RequestRetry {
        message: message.into(),
    };
    timeline.apply_at(Some(1), &retry("Reconnecting... 1/5"));
    timeline.apply_at(Some(2), &retry("Reconnecting... 2/5"));
    timeline.apply_at(
        Some(3),
        &AgentEvent::Warning {
            message: "fell back".into(),
        },
    );
    timeline.apply_at(Some(4), &retry("Reconnecting... 1/5"));

    let shown: Vec<_> = timeline
        .entries
        .iter()
        .map(|entry| match &entry.content {
            EntryContent::RequestRetry { message } => message.as_str(),
            EntryContent::Warning { .. } => "warning",
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        shown,
        ["Reconnecting... 2/5", "warning", "Reconnecting... 1/5"]
    );
    assert_ne!(timeline.entries[0].id, timeline.entries[2].id);
}

#[test]
fn usage_limit_reset_is_folded_into_the_latest_error() {
    let mut timeline = Timeline::default();
    timeline.apply_at(
        None,
        &AgentEvent::Error {
            message: "boom".into(),
            fatal: false,
        },
    );
    timeline.apply_at(None, &AgentEvent::UsageLimitReached { resets_at: 42 });

    assert!(matches!(
        &timeline.entries[0].content,
        EntryContent::Error {
            message,
            limit_resets_at: Some(42),
        } if message == "boom"
    ));
}

#[test]
fn errors_and_session_close_fold_into_timeline() {
    let mut timeline = Timeline::default();
    timeline.apply_at(
        None,
        &AgentEvent::TurnStarted {
            turn_id: "t".into(),
        },
    );
    timeline.apply_at(
        None,
        &AgentEvent::ApprovalRequested(ApprovalRequest {
            id: "req".into(),
            parent_item_id: None,
            turn_id: None,
            kind: ApprovalKind::ExecCommand {
                command: "rm -rf /".into(),
                cwd: None,
                description: None,
            },
            reason: None,
            blocked_path: None,
            options: Vec::new(),
        }),
    );
    timeline.apply_at(
        None,
        &AgentEvent::Error {
            message: "boom".into(),
            fatal: true,
        },
    );
    timeline.apply_at(None, &AgentEvent::SessionClosed { reason: None });

    assert!(!timeline.turn_running);
    assert!(timeline.pending_approvals.is_empty());
    assert!(matches!(
        &timeline.entries[0].content,
        EntryContent::Error { message, .. } if message == "boom"
    ));
    // A silent close (reason: None) leaves no entry…
    let entries_after_silent_close = timeline.entries.len();
    // …but an abnormal close records the provider's dying words.
    timeline.apply_at(
        None,
        &AgentEvent::SessionClosed {
            reason: Some("codex app-server exited with exit status: 1\nstderr:\nboom".into()),
        },
    );
    assert_eq!(timeline.entries.len(), entries_after_silent_close + 1);
    assert!(matches!(
        &timeline.entries.last().unwrap().content,
        EntryContent::Error { message, .. } if message.contains("stderr:\nboom")
    ));
}

#[test]
fn parented_items_are_dropped_while_subagent_spawn_status_merges() {
    let spawn = ThreadItem {
        id: "spawn".into(),
        parent_item_id: None,
        content: ItemContent::Subagent {
            agent_type: "general-purpose".into(),
            description: "Ping test".into(),
            status: ItemStatus::InProgress,
            summary: None,
            model: Some("opus".into()),
            effort: None,
        },
    };
    let child = ThreadItem {
        id: "spawn:user-1".into(),
        parent_item_id: Some("spawn".into()),
        content: ItemContent::UserMessage {
            text: "ping".into(),
            attachments: Vec::new(),
        },
    };
    let completed = ThreadItem {
        content: ItemContent::Subagent {
            agent_type: "general-purpose".into(),
            description: "Ping test".into(),
            status: ItemStatus::Completed,
            summary: Some("pong".into()),
            model: None,
            effort: Some("high".into()),
        },
        ..spawn.clone()
    };
    let timeline = Timeline::fold_events([
        AgentEvent::ItemStarted(spawn),
        AgentEvent::ItemCompleted(child),
        AgentEvent::ItemCompleted(completed),
    ]);
    assert_eq!(timeline.entries.len(), 1);
    assert_eq!(timeline.entries[0].id, "spawn");
    assert!(matches!(
        &timeline.entries[0].content,
        EntryContent::Item(ItemContent::Subagent {
            status: ItemStatus::Completed,
            summary: Some(summary),
            model: Some(model),
            effort: Some(effort),
            ..
        }) if summary == "pong" && model == "opus" && effort == "high"
    ));
}

/// A message stored while Tcode still marked a context prefix in it
/// reads, and reads as its whole text: the stored length is ignored.
#[test]
fn a_stored_user_message_with_a_context_length_reads_as_its_whole_text() {
    let record = r#"{"type":"item_completed","id":"u1","content":{"kind":"user_message","text":"PREFIX\n\nvisible","context_len":8}}"#;
    let event: AgentEvent = serde_json::from_str(record).unwrap();
    let timeline = Timeline::fold_events([event]);
    assert!(matches!(&timeline.entries[0].content,
            EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "PREFIX\n\nvisible"));
}

fn at(ts: u64, event: AgentEvent) -> StoredEvent {
    StoredEvent {
        origin: None,
        author: None,
        ts: Some(ts),
        event,
        elided: None,
    }
}

fn started(ts: u64, item: ThreadItem) -> StoredEvent {
    at(ts, AgentEvent::ItemStarted(item))
}

fn updated(ts: u64, item: ThreadItem) -> StoredEvent {
    at(ts, AgentEvent::ItemUpdated(item))
}

fn completed(ts: u64, item: ThreadItem) -> StoredEvent {
    at(ts, AgentEvent::ItemCompleted(item))
}

fn command(id: &str, status: ItemStatus) -> ThreadItem {
    ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: ItemContent::CommandExecution {
            command: "ls".into(),
            input: Vec::new(),
            output: String::new(),
            exit_code: None,
            status,
        },
    }
}

/// A shell command that is still running.
fn running(id: &str) -> ThreadItem {
    command(id, ItemStatus::InProgress)
}

/// The same command, finished.
fn ran(id: &str) -> ThreadItem {
    command(id, ItemStatus::Completed)
}

fn subagent(id: &str, status: ItemStatus) -> ThreadItem {
    ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: ItemContent::Subagent {
            agent_type: "explore".into(),
            description: "look around".into(),
            status,
            summary: None,
            model: None,
            effort: None,
        },
    }
}

fn assistant(id: &str, text: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        },
    })
}

/// A statusless tool-like item: its lifecycle is only knowable from the
/// event variant that carried it (Codex maps `webSearch` this way).
fn web_search(id: &str) -> ThreadItem {
    ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: ItemContent::WebSearch {
            query: "rust union of intervals".into(),
        },
    }
}

/// The other statusless shape: a provider item canonicalization does not
/// model yet.
fn other_item(id: &str) -> ThreadItem {
    ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: ItemContent::Other {
            provider_kind: "customTool".into(),
            summary: "doing something".into(),
        },
    }
}

fn turn_started() -> AgentEvent {
    AgentEvent::TurnStarted {
        turn_id: "turn-1".into(),
    }
}

fn turn_completed() -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: "turn-1".into(),
        status: TurnStatus::Completed,
        usage: None,
    }
}

/// Fold timestamped events and return the first turn's breakdown.
fn timing_of(events: Vec<StoredEvent>) -> Option<TurnTiming> {
    Timeline::fold_events(events).turns[0].timing
}

#[test]
fn tool_timing_follows_lifecycle_including_updates_failures_and_reused_ids() {
    for (label, activity, expected_tool_ms) in [
        (
            "sequential",
            vec![
                started(1_000, running("a")),
                completed(2_000, ran("a")),
                started(3_000, running("b")),
                completed(4_000, ran("b")),
            ],
            2_000,
        ),
        (
            "overlapping and nested",
            vec![
                started(1_000, running("a")),
                started(1_500, subagent("b", ItemStatus::InProgress)),
                started(1_800, running("c")),
                completed(2_000, ran("c")),
                completed(2_500, ran("a")),
                completed(3_000, subagent("b", ItemStatus::Completed)),
            ],
            2_000,
        ),
        (
            "updates and reused id",
            vec![
                started(1_000, running("a")),
                updated(1_400, running("a")),
                updated(2_200, running("a")),
                completed(3_000, ran("a")),
                started(4_000, running("a")),
                completed(4_500, ran("a")),
            ],
            2_500,
        ),
        (
            "missing start",
            vec![updated(1_000, running("a")), completed(2_500, ran("a"))],
            1_500,
        ),
        (
            "backward completion followed by clock recovery",
            vec![
                started(2_000, running("a")),
                completed(1_000, ran("a")),
                started(3_000, running("b")),
                completed(5_000, ran("b")),
            ],
            2_000,
        ),
        (
            "failed",
            vec![
                started(1_000, running("a")),
                updated(2_000, command("a", ItemStatus::Failed)),
            ],
            1_000,
        ),
        (
            "start carries terminal status",
            vec![started(1_000, ran("a")), completed(3_000, ran("a"))],
            2_000,
        ),
        (
            "statusless web search",
            vec![
                started(2_000, web_search("a")),
                updated(3_000, web_search("a")),
                completed(4_500, web_search("a")),
            ],
            2_500,
        ),
        (
            "statusless provider item",
            vec![
                started(2_000, other_item("a")),
                updated(3_000, other_item("a")),
                completed(4_500, other_item("a")),
            ],
            2_500,
        ),
    ] {
        let mut events = vec![at(0, turn_started())];
        events.extend(activity);
        events.push(at(6_000, turn_completed()));
        let timing = timing_of(events).unwrap_or_else(|| panic!("{label}: missing timing"));
        assert_eq!(
            (timing.total_ms, timing.tool_ms),
            (6_000, expected_tool_ms),
            "{label}"
        );
    }
}

#[test]
fn unobserved_unfinished_or_inconsistent_turns_have_no_timing_breakdown() {
    for (label, events) in [
        (
            "legacy",
            vec![
                user_msg("u", "hi").into(),
                turn_started().into(),
                AgentEvent::ItemStarted(running("a")).into(),
                AgentEvent::ItemCompleted(ran("a")).into(),
                turn_completed().into(),
            ],
        ),
        (
            "untimed tool",
            vec![
                at(1_000, turn_started()),
                AgentEvent::ItemStarted(running("a")).into(),
                AgentEvent::ItemCompleted(ran("a")).into(),
                at(9_000, turn_completed()),
            ],
        ),
        (
            "running",
            vec![at(1_000, turn_started()), started(2_000, running("a"))],
        ),
        (
            "no observed start",
            vec![
                at(1_000, user_msg("u", "go")),
                started(2_000, running("a")),
                completed(3_000, ran("a")),
                at(9_000, turn_completed()),
            ],
        ),
        (
            "untimed start",
            vec![
                at(1_000, user_msg("u", "go")),
                turn_started().into(),
                at(9_000, turn_completed()),
            ],
        ),
        (
            "no end timestamp",
            vec![at(1_000, turn_started()), turn_completed().into()],
        ),
        (
            "end before start",
            vec![at(9_000, turn_started()), at(1_000, turn_completed())],
        ),
        (
            "tool beyond end",
            vec![
                at(0, turn_started()),
                started(1_000, running("a")),
                completed(2_000, ran("a")),
                started(19_000, running("b")),
                completed(25_000, ran("b")),
                at(20_000, turn_completed()),
            ],
        ),
        (
            "assistant beyond end",
            vec![
                at(1_000, turn_started()),
                at(30_000, assistant("a", "late")),
                at(20_000, turn_completed()),
            ],
        ),
        (
            "reasoning beyond end",
            vec![
                at(1_000, turn_started()),
                at(
                    30_000,
                    AgentEvent::Delta {
                        item_id: "r".into(),
                        kind: DeltaKind::ReasoningText,
                        text: "late".into(),
                    },
                ),
                at(20_000, turn_completed()),
            ],
        ),
    ] {
        assert_eq!(timing_of(events), None, "{label}");
    }
    let timeline =
        Timeline::fold_events([at(1_000, user_msg("u", "go")), at(9_000, turn_completed())]);
    assert_eq!(timeline.turns[0].start_ts, Some(1_000));
    assert_eq!(timeline.turns[0].timing, None);
}

#[test]
fn timing_charges_only_tool_intervals_inside_the_observed_turn() {
    for (label, activity, expected_tool_ms) in [
        (
            "before turn",
            vec![
                started(1_100, running("a")),
                completed(2_000, ran("a")),
                at(5_000, turn_started()),
            ],
            0,
        ),
        (
            "straddling start",
            vec![
                started(1_100, running("a")),
                at(5_000, turn_started()),
                completed(6_000, ran("a")),
            ],
            1_000,
        ),
        (
            "unfinished tool",
            vec![at(5_000, turn_started()), started(6_000, running("a"))],
            3_000,
        ),
        (
            "model only",
            vec![
                at(5_000, turn_started()),
                at(6_000, assistant("a", "answer")),
            ],
            0,
        ),
    ] {
        let mut events = vec![at(1_000, user_msg("u1", "go"))];
        events.extend(activity);
        events.push(at(9_000, turn_completed()));
        let timing = timing_of(events).unwrap_or_else(|| panic!("{label}: missing timing"));
        assert_eq!(
            (timing.total_ms, timing.tool_ms),
            (4_000, expected_tool_ms),
            "{label}"
        );
    }
}

#[test]
fn a_turn_finalized_without_a_timestamp_rejects_later_tool_transitions() {
    for (label, first_end, first_timing) in [
        (
            "timed",
            Some(4_000),
            Some(TurnTiming {
                total_ms: 4_000,
                tool_ms: 1_000,
            }),
        ),
        ("untimed", None, None),
    ] {
        let mut timeline = Timeline::fold_events([
            at(0, turn_started()),
            started(1_000, running("a")),
            completed(2_000, ran("a")),
            StoredEvent {
                origin: None,
                author: None,
                ts: first_end,
                event: turn_completed(),
                elided: None,
            },
        ]);
        assert_eq!(timeline.turns[0].timing, first_timing, "{label}");
        for event in [
            started(4_500, running("ghost")),
            at(5_000, user_msg("u2", "again")),
            at(5_000, turn_started()),
            at(9_000, turn_completed()),
        ] {
            timeline.apply_stored(&event);
        }
        assert_eq!(timeline.turns.len(), 2, "{label}");
        assert_eq!(timeline.turns[0].timing, first_timing, "{label}");
        assert_eq!(
            timeline.turns[1].timing,
            Some(TurnTiming {
                total_ms: 4_000,
                tool_ms: 0,
            }),
            "{label}"
        );
    }
}

#[test]
fn served_model_change_sets_turn_meta_and_adds_one_divider() {
    for (requested, transitions, served, expected) in [
        (
            "requested",
            vec![
                ("served", Some("requested"), Some("capacity")),
                ("served", Some("requested"), Some("capacity")),
            ],
            "served",
            vec![(Some("requested"), "served", Some("capacity"))],
        ),
        (
            "claude-opus-5[1m]",
            vec![("claude-opus-5", None, None)],
            "claude-opus-5",
            vec![],
        ),
        (
            "claude-opus-5[1m]",
            vec![
                ("claude-opus-5", None, None),
                ("claude-opus-5[3m]", Some("claude-opus-5"), None),
            ],
            "claude-opus-5[3m]",
            vec![(Some("claude-opus-5"), "claude-opus-5[3m]", None)],
        ),
    ] {
        let mut timeline = Timeline::fold_events([
            AgentEvent::SessionStarted {
                provider_session_id: "session".into(),
                resume: ResumeCursor(json!({})),
                model: Some(requested.into()),
            },
            user_msg("user", "hello"),
        ]);
        for (model, from, reason) in transitions {
            timeline.apply_at(
                None,
                &AgentEvent::ServedModel {
                    model: model.into(),
                    from: from.map(str::to_string),
                    reason: reason.map(str::to_string),
                },
            );
        }
        assert_eq!(timeline.turns[0].served_model.as_deref(), Some(served));
        let changes = timeline
            .entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::ModelChanged { from, to, reason } => {
                    Some((from.as_deref(), to.as_str(), reason.as_deref()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(changes, expected, "{requested} -> {served}");
    }
}

/// Logs recorded before pi named its streamed messages carry every
/// turn's assistant stream under the same placeholder id. A stream
/// continues the open turn's item; the earlier turn's entry is a
/// different message, and a suffix of the log (as history pages load)
/// must fold its turns exactly like the whole log does.
#[test]
fn streamed_deltas_follow_background_items_and_keep_reused_replies_in_their_turns() {
    let turn = |n: u64| {
        [
            StoredEvent {
                origin: None,
                author: None,
                ts: Some(n * 10),
                event: user_msg(&format!("user-{n}"), "go"),
                elided: None,
            },
            StoredEvent {
                origin: None,
                author: None,
                ts: Some(n * 10 + 1),
                event: AgentEvent::TurnStarted {
                    turn_id: format!("turn-{n}"),
                },
                elided: None,
            },
            StoredEvent {
                origin: None,
                author: None,
                ts: Some(n * 10 + 2),
                event: assistant_delta("pi-assistant-0:0", &format!("text {n}")),
                elided: None,
            },
            StoredEvent {
                origin: None,
                author: None,
                ts: Some(n * 10 + 3),
                event: AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{n}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
                elided: None,
            },
        ]
    };
    let mut log = Vec::new();
    for n in 1..=2 {
        let events = turn(n);
        log.extend_from_slice(&events[..3]);
        if n == 1 {
            log.push(AgentEvent::ItemStarted(running("background-command")).into());
            log.push(
                AgentEvent::ItemBackgrounded {
                    item_id: "background-command".into(),
                    parent_item_id: None,
                }
                .into(),
            );
        }
        log.push(
            AgentEvent::Delta {
                item_id: "background-command".into(),
                kind: DeltaKind::CommandOutput,
                text: format!("output {n}\n"),
            }
            .into(),
        );
        log.push(events[3].clone());
    }
    log.push(
        AgentEvent::ItemCompleted(command("background-command", ItemStatus::Interrupted)).into(),
    );
    let full = Timeline::fold_events(log.iter().cloned());
    assert_eq!(
        full.entries
            .iter()
            .filter(|entry| entry.id == "pi-assistant-0:0")
            .map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::AssistantMessage { text, .. }) =>
                    (entry.turn, text.as_str()),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>(),
        [(0, "text 1"), (1, "text 2")],
        "each turn keeps its own streamed message"
    );

    assert_eq!(
        full.entries
            .iter()
            .filter(|entry| entry.id == "background-command")
            .map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::CommandExecution { output, status, .. }) =>
                    (entry.turn, output.as_str(), *status),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>(),
        [(0, "output 1\noutput 2\n", ItemStatus::Interrupted)],
        "later output and completion belong to the original background command"
    );

    let log: Vec<StoredEvent> = [turn(1), turn(2)].concat();
    let full = Timeline::fold_events(log.iter().cloned());
    let suffix = Timeline::fold_events(log[4..].iter().cloned());
    let entries = |timeline: &Timeline, skip: usize| {
        timeline.entries[skip..]
            .iter()
            .map(|entry| {
                (
                    entry.id.clone(),
                    entry.turn - timeline.entries[skip].turn,
                    format!("{:?}", entry.content),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        entries(&suffix, 0),
        entries(&full, full.entries.len() - suffix.entries.len()),
        "a page boundary at a turn start folds the same turns as the full log"
    );
}

/// The ACP session (`acp_session.rs`, shared by ACP agents, Cursor and
/// Grok) names text blocks `thought-N` / `msg-N` from a counter that
/// starts at zero in every provider process, so the first turn after a
/// restart (app relaunch, or the one-hour idle reap) streams and completes
/// the same ids the thread's first process used. The recorded Grok
/// `resumed_turn.jsonl` shows a resumed process's first turn as
/// `thought-1`, `msg-2`. Each turn must keep its own reply.
#[test]
fn a_restarted_provider_reusing_an_earlier_turns_item_ids_keeps_both_turns_replies() {
    let reasoning = |id: &str, text: &str| {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::Reasoning { text: text.into() },
        })
    };
    let process_turn = |n: u64, answer: &str| -> Vec<StoredEvent> {
        [
            user_msg(&format!("user-{n}"), "go"),
            // `turn_seq` restarts with the process too.
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            AgentEvent::Delta {
                item_id: "thought-1".into(),
                kind: DeltaKind::ReasoningText,
                text: format!("thinking {n}"),
            },
            reasoning("thought-1", &format!("thinking {n}")),
            assistant_delta("msg-2", answer),
            assistant("msg-2", answer),
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ]
        .into_iter()
        .enumerate()
        .map(|(i, event)| at(n * 100 + i as u64, event))
        .collect()
    };
    let log = [
        process_turn(1, "first answer"),
        process_turn(2, "second answer"),
    ]
    .concat();
    let timeline = Timeline::fold_events(log.iter().cloned());
    let rendered = |timeline: &Timeline| {
        timeline
            .entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::AssistantMessage { text, .. })
                | EntryContent::Item(ItemContent::Reasoning { text }) => Some((
                    entry.turn,
                    entry.id.clone(),
                    text.clone(),
                    entry.finished_ts.is_some(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        rendered(&timeline),
        [
            (0, "thought-1".into(), "thinking 1".into(), true),
            (0, "msg-2".into(), "first answer".into(), true),
            (1, "thought-1".into(), "thinking 2".into(), true),
            (1, "msg-2".into(), "second answer".into(), true),
        ]
    );
}

#[test]
fn synthetic_entry_ids_do_not_depend_on_how_much_earlier_history_is_folded() {
    let error = |ts: u64| StoredEvent {
        origin: None,
        author: None,
        ts: Some(ts),
        event: AgentEvent::Error {
            message: format!("boom {ts}"),
            fatal: false,
        },
        elided: None,
    };
    let log = [
        StoredEvent {
            origin: None,
            author: None,
            ts: Some(1),
            event: user_msg("user-1", "go"),
            elided: None,
        },
        error(2),
        StoredEvent {
            origin: None,
            author: None,
            ts: Some(3),
            event: AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Failed,
                usage: None,
            },
            elided: None,
        },
        StoredEvent {
            origin: None,
            author: None,
            ts: Some(4),
            event: user_msg("user-2", "again"),
            elided: None,
        },
        error(5),
        error(5),
    ];
    let full = Timeline::fold_events(log.iter().cloned());
    let suffix = Timeline::fold_events(log[3..].iter().cloned());
    let ids = |timeline: &Timeline| {
        timeline
            .entries
            .iter()
            .filter(|entry| matches!(entry.content, EntryContent::Error { .. }))
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&full), ["error-2", "error-5", "error-5-1"]);
    assert_eq!(ids(&suffix), ["error-5", "error-5-1"]);

    let legacy = Timeline::fold_events([
        AgentEvent::Error {
            message: "old".into(),
            fatal: false,
        },
        AgentEvent::Error {
            message: "older".into(),
            fatal: false,
        },
    ]);
    assert_eq!(ids(&legacy), ["error-1", "error-2"]);
}
