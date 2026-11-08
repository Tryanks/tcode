use agent::{
    AgentEvent, ForkPoint, ItemContent, ItemStatus, ProviderKind, RewindMode, ThreadItem,
    TurnStatus,
};
use serde_json::{Value, json};
use tcode_core::project::SessionMeta;
use tcode_core::session::EntryContent;
use tcode_protocol::{Command, CommandResponse, HostMessage, ProtocolError};

use super::*;
use crate::app::scripted_provider;
use crate::app::test_support::*;

const SOURCE: &str = "source";

fn host(store: &TestStore, cx: &mut TestAppContext) -> TestEntity {
    cx.new_entity(TestClientState::new((**store).clone()))
}

/// A thread of `provider`, persisted and resident.
fn install(state: &mut AppState, provider: ProviderKind, cwd: PathBuf, cx: &mut HostCx) {
    let mut meta = SessionMeta::new(provider, cwd, None);
    meta.id = SOURCE.into();
    meta.title = "The source".into();
    meta.resume_cursor = Some(agent::ResumeCursor(json!({ "session_id": "native" })));
    state.sessions.push(meta.clone());
    state.persist_meta(&meta, cx);
    state
        .residents
        .live
        .insert(meta.id.clone(), ActiveSession::new(meta, false, Vec::new()));
}

fn turn(state: &mut AppState, cx: &mut HostCx, index: usize, prompt: &str, attachments: &[&str]) {
    let turn_id = format!("turn-{index}");
    for event in [
        AgentEvent::ItemCompleted(ThreadItem {
            id: format!("user-{index}"),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: prompt.into(),
                context_len: None,
                attachments: attachments.iter().map(|path| (*path).to_owned()).collect(),
            },
        }),
        AgentEvent::TurnStarted {
            turn_id: turn_id.clone(),
        },
        AgentEvent::TurnCheckpoint {
            turn_id: turn_id.clone(),
            checkpoint_id: format!("checkpoint-{index}"),
        },
        AgentEvent::ItemCompleted(ThreadItem {
            id: format!("answer-{index}"),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: format!("answer to {prompt}"),
                summarized: false,
            },
        }),
        AgentEvent::TurnCompleted {
            turn_id,
            status: TurnStatus::Completed,
            usage: None,
        },
    ] {
        state.record_event(SOURCE, &event, cx);
    }
}

fn rewound(checkpoint: &str, later_history: agent::LaterHistory, undo: Option<&str>) -> AgentEvent {
    AgentEvent::RewindCompleted {
        checkpoint_id: checkpoint.into(),
        mode: RewindMode::Conversation,
        prefill: None,
        prefill_attachments: Vec::new(),
        later_history,
        files: None,
        incomplete: None,
        undo_token: undo.map(str::to_owned),
    }
}

/// The client's message, as it comes off the wire, and the host's answer.
fn send(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    command: Value,
) -> Result<CommandResponse, ProtocolError> {
    let command: Command = serde_json::from_value(command).unwrap();
    state.dispatch_command(cx, id, command);
    cx.run_until_sent(|message| match message {
        HostMessage::Ack { id: acked, result } if *acked == id => Some(result.clone()),
        _ => None,
    })
    .0
}

fn fork_at(turn: usize, checkpoint: &str) -> Value {
    json!({"type": "fork_thread", "content": {
        "id": SOURCE, "at": {"turn": turn, "checkpoint_id": checkpoint},
    }})
}

/// The thread a fork answers with.
fn forked(response: Result<CommandResponse, ProtocolError>) -> String {
    match response {
        Ok(CommandResponse::SessionId(Some(id))) => id,
        other => panic!("{other:?}"),
    }
}

fn open_branch(turn: usize, checkpoint: &str) -> Value {
    json!({"type": "open_rewound_branch", "content": {
        "session_id": SOURCE, "turn": turn, "checkpoint_id": checkpoint,
    }})
}

fn prompts(session: &ActiveSession) -> Vec<String> {
    session
        .timeline
        .entries
        .iter()
        .filter_map(|entry| match &entry.content {
            EntryContent::Item(ItemContent::UserMessage { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Run until the thread `id` is resident with its log loaded.
fn settled(cx: &mut TestAppContext, id: &str) {
    cx.run_until(|state| state.residents.live.contains_key(id));
    cx.run_until_parked();
}

/// Through a turn: the new thread holds exactly the turns through it, its
/// answer included, with nothing for a composer; the source, with a turn
/// running in it, is as it was; the Harness makes the fork at the first send,
/// from the next prompt's restore point; and the thread says where it came
/// from.
#[test]
fn a_fork_through_a_turn_holds_it_and_what_came_before_and_leaves_the_source_alone() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-fork-at");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let launches = scripted.launches.clone();
    let events = scripted.events.clone();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*store).clone());
        state.set_provider_launcher_for_test(scripted.launcher);
        state
    });
    state.update(cx, |state, cx| {
        install(state, ProviderKind::ClaudeCode, cwd.clone(), cx);
        turn(state, cx, 0, "first", &[]);
        turn(state, cx, 1, "second", &["/images/second.png"]);
        turn(state, cx, 2, "third", &[]);
        state.record_event(
            SOURCE,
            &AgentEvent::TurnStarted {
                turn_id: "turn-running".into(),
            },
            cx,
        );
    });
    cx.run_until_parked();
    let source_log = store.read_event_log(SOURCE).unwrap();

    let session_id = forked(send(&state, cx, 1, fork_at(1, "checkpoint-1")));
    settled(cx, &session_id);

    state.read(|state| {
        let fork = &state.residents.live[&session_id];
        assert_eq!(prompts(fork), ["first", "second"]);
        assert!(matches!(
            &fork.timeline.entries.last().unwrap().content,
            EntryContent::Item(ItemContent::AssistantMessage { text, .. })
                if text == "answer to second"
        ));
        assert_eq!(fork.timeline.turns.len(), 2);
        assert_eq!(
            fork.meta.pending_fork,
            Some(ForkPoint::Before {
                checkpoint_id: "checkpoint-2".into()
            })
        );
        let origin = fork.meta.fork_origin.as_ref().unwrap();
        assert_eq!(origin.thread_id, SOURCE);
        assert_eq!(origin.title, "The source");
        let source = &state.residents.live[SOURCE];
        assert_eq!(prompts(source), ["first", "second", "third"]);
        assert!(source.timeline.turn_running, "the source's turn runs on");
    });
    assert_eq!(store.read_event_log(SOURCE).unwrap(), source_log);
    assert!(
        store
            .read_event_log(&session_id)
            .unwrap()
            .starts_with(&source_log[..source_log.len() / 8]),
        "the fork's log is the source's own rows"
    );
    assert_eq!(
        store.turn_index(&session_id).unwrap().unwrap().starts.len(),
        2,
        "windows of the fork's log begin at its own turns"
    );

    send(
        &state,
        cx,
        2,
        json!({"type": "send_turn", "content": {
            "session_id": session_id, "text": "something else", "attachment_paths": [],
        }}),
    )
    .unwrap();
    cx.run_until(|_| !launches.is_empty());
    let launch = launches.try_recv().unwrap();
    assert_eq!(
        launch.fork,
        Some(ForkPoint::Before {
            checkpoint_id: "checkpoint-2".into()
        })
    );
    assert_eq!(
        launch.resume,
        Some(agent::ResumeCursor(json!({ "session_id": "native" })))
    );
    drop(events);
}

/// With no prompt after it the turn's fork is all of the session, for every
/// Harness that forks through a turn.
#[test]
fn a_fork_through_the_latest_turn_is_the_whole_session() {
    for provider in [
        ProviderKind::ClaudeCode,
        ProviderKind::Codex,
        ProviderKind::Pi,
        ProviderKind::OpenCode,
        ProviderKind::Grok,
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-fork-latest");
        let cwd = store.root().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let state = host(&store, cx);
        state.update(cx, |state, cx| {
            install(state, provider, cwd, cx);
            turn(state, cx, 0, "first", &[]);
            turn(state, cx, 1, "second", &[]);
        });
        cx.run_until_parked();
        let fork = forked(send(&state, cx, 1, fork_at(1, "checkpoint-1")));
        settled(cx, &fork);
        state.read(|state| {
            let fork = &state.residents.live[&fork];
            assert_eq!(prompts(fork), ["first", "second"], "{provider:?}");
            assert_eq!(
                fork.meta.pending_fork,
                Some(ForkPoint::Tail),
                "{provider:?}"
            );
        });
    }
}

/// Where a fork is refused: a restore point other than the client saw, a
/// Harness that has no fork through a turn, a turn still running, a turn
/// whose next prompt has no restore point yet, and a last turn with a prompt
/// on its way to the Harness.
#[test]
fn a_fork_through_a_turn_goes_only_where_the_harness_can() {
    #[derive(Clone, Copy)]
    enum Then {
        Nothing,
        Prompt { point: bool },
        InFlight,
    }
    const RUNNING: Then = Then::Prompt { point: true };
    for (provider, turn_index, checkpoint, then, forks) in [
        (
            ProviderKind::ClaudeCode,
            0,
            "checkpoint-0",
            Then::Nothing,
            true,
        ),
        (
            ProviderKind::ClaudeCode,
            1,
            "checkpoint-stale",
            Then::Nothing,
            false,
        ),
        (ProviderKind::Pi, 0, "checkpoint-0", Then::Nothing, true),
        (
            ProviderKind::Cursor,
            1,
            "checkpoint-1",
            Then::Nothing,
            false,
        ),
        (ProviderKind::ClaudeCode, 2, "checkpoint-2", RUNNING, false),
        (ProviderKind::ClaudeCode, 1, "checkpoint-1", RUNNING, true),
        (
            ProviderKind::ClaudeCode,
            1,
            "checkpoint-1",
            Then::Prompt { point: false },
            false,
        ),
        (
            ProviderKind::ClaudeCode,
            1,
            "checkpoint-1",
            Then::InFlight,
            false,
        ),
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-fork-at-refused");
        let cwd = store.root().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let state = host(&store, cx);
        state.update(cx, |state, cx| {
            install(state, provider, cwd, cx);
            turn(state, cx, 0, "first", &[]);
            turn(state, cx, 1, "second", &[]);
            match then {
                Then::Nothing => {}
                Then::InFlight => state.resident_mut(SOURCE).unwrap().turn_in_flight = true,
                Then::Prompt { point } => {
                    let prompt = AgentEvent::ItemCompleted(ThreadItem {
                        id: "user-2".into(),
                        parent_item_id: None,
                        content: ItemContent::UserMessage {
                            text: "third".into(),
                            context_len: None,
                            attachments: Vec::new(),
                        },
                    });
                    state.record_event(SOURCE, &prompt, cx);
                    let started = AgentEvent::TurnStarted {
                        turn_id: "turn-2".into(),
                    };
                    state.record_event(SOURCE, &started, cx);
                    if point {
                        let checkpoint = AgentEvent::TurnCheckpoint {
                            turn_id: "turn-2".into(),
                            checkpoint_id: "checkpoint-2".into(),
                        };
                        state.record_event(SOURCE, &checkpoint, cx);
                    }
                }
            }
        });
        cx.run_until_parked();
        match send(&state, cx, 1, fork_at(turn_index, checkpoint)) {
            Ok(CommandResponse::SessionId(Some(_))) => assert!(forks, "{provider:?} {turn_index}"),
            Err(error) => {
                assert!(!forks, "{provider:?} {turn_index}");
                assert_eq!(error.code, "fork_unavailable");
            }
            other => panic!("{other:?}"),
        }
    }
}

/// The turn a rewind's divider opened starts after the rows of what the rewind
/// took out: a fork through the turn before it holds what the rewind left,
/// not them. With nothing sent after the rewind that is the whole session,
/// unless the Harness keeps the removed turns until the next send.
#[test]
fn a_fork_after_a_rewind_holds_what_the_rewind_left() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-fork-after-rewind");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = host(&store, cx);
    state.update(cx, |state, cx| {
        install(state, ProviderKind::ClaudeCode, cwd, cx);
        turn(state, cx, 0, "first", &[]);
        turn(state, cx, 1, "taken out", &[]);
        state.record_event(
            SOURCE,
            &rewound("checkpoint-1", agent::LaterHistory::Truncated, None),
            cx,
        );
        turn(state, cx, 2, "sent again", &[]);
    });
    cx.run_until_parked();
    let session_id = forked(send(&state, cx, 1, fork_at(0, "checkpoint-0")));
    settled(cx, &session_id);
    state.read(|state| {
        let fork = &state.residents.live[&session_id];
        assert_eq!(prompts(fork), ["first"]);
        assert_eq!(
            fork.timeline.turns.len(),
            2,
            "the divider's turn, then the next"
        );
    });

    for (later_history, undo, forks) in [
        (agent::LaterHistory::Truncated, None, true),
        (
            agent::LaterHistory::HiddenUntilNextSend,
            Some("revert"),
            false,
        ),
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-fork-after-rewind-unsent");
        let cwd = store.root().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let state = host(&store, cx);
        state.update(cx, |state, cx| {
            install(state, ProviderKind::ClaudeCode, cwd, cx);
            turn(state, cx, 0, "first", &[]);
            turn(state, cx, 1, "taken out", &[]);
            state.record_event(SOURCE, &rewound("checkpoint-1", later_history, undo), cx);
        });
        cx.run_until_parked();
        let answer = send(&state, cx, 1, fork_at(0, "checkpoint-0"));
        assert_eq!(answer.is_ok(), forks, "{later_history:?}");
    }
}

/// A forked thread that has not started yet stands at the point it was made
/// from: forking it again, whole or through its last turn, starts from there,
/// not from the end of the session both resume.
#[test]
fn a_fork_of_a_fork_not_started_keeps_its_point() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-fork-of-fork");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = host(&store, cx);
    state.update(cx, |state, cx| {
        install(state, ProviderKind::ClaudeCode, cwd, cx);
        turn(state, cx, 0, "first", &[]);
        turn(state, cx, 1, "second", &[]);
    });
    cx.run_until_parked();
    let session_id = forked(send(&state, cx, 1, fork_at(0, "checkpoint-0")));
    settled(cx, &session_id);
    let again = [
        json!({"type": "fork_thread", "content": {"id": session_id}}),
        json!({"type": "fork_thread", "content": {"id": session_id, "at": {"turn": 0}}}),
    ];
    for (id, command) in (2..).zip(again) {
        let again = forked(send(&state, cx, id, command));
        settled(cx, &again);
        state.read(|state| {
            assert_eq!(
                state.residents.live[&again].meta.pending_fork,
                Some(ForkPoint::Before {
                    checkpoint_id: "checkpoint-1".into()
                })
            );
        });
    }
}

/// The branch a rewind left is the thread's to return to until the next send,
/// and any time after that as a thread of its own: it holds the branch's
/// turns and restore points, goes on in the Harness's kept session, and
/// asking again finds it rather than making a second thread on one session.
/// A fork, a session of its own, does not carry the way to the branch.
#[test]
fn the_branch_a_rewind_left_opens_as_a_thread_of_its_own_after_the_next_send() {
    const LEFT: &str = "/sessions/left.jsonl";
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-open-branch");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = host(&store, cx);
    state.update(cx, |state, cx| {
        install(state, ProviderKind::Pi, cwd, cx);
        turn(state, cx, 0, "first", &[]);
        turn(state, cx, 1, "second", &[]);
        turn(state, cx, 2, "third", &[]);
        state.record_event(
            SOURCE,
            &rewound("checkpoint-1", agent::LaterHistory::Branch, Some(LEFT)),
            cx,
        );
    });
    cx.run_until_parked();

    let refused = send(&state, cx, 1, open_branch(1, "checkpoint-1")).unwrap_err();
    assert_eq!(refused.code, "rewind_unavailable", "the undo is still open");

    state.update(cx, |state, cx| {
        turn(state, cx, 3, "second, edited", &[]);
        turn(state, cx, 4, "and on", &[]);
    });
    cx.run_until_parked();
    let source_log = store.read_event_log(SOURCE).unwrap();
    let CommandResponse::SessionId(Some(branch)) =
        send(&state, cx, 2, open_branch(1, "checkpoint-1")).unwrap()
    else {
        panic!("the branch answers with its thread");
    };
    settled(cx, &branch);
    state.read(|state| {
        let thread = &state.residents.live[&branch];
        assert_eq!(prompts(thread), ["first", "second", "third"]);
        assert_eq!(
            thread.meta.pending_fork,
            Some(ForkPoint::Branch { token: LEFT.into() })
        );
        let origin = thread.meta.fork_origin.as_ref().unwrap();
        assert_eq!(origin.thread_id, SOURCE);
        assert_eq!(origin.branch.as_deref(), Some(LEFT));
        assert_eq!(
            thread.timeline.turns[1].provider_checkpoint_id.as_deref(),
            Some("checkpoint-1"),
            "the same session, so its restore points still address it"
        );
        assert_eq!(
            prompts(&state.residents.live[SOURCE]),
            ["first", "second, edited", "and on"]
        );
    });
    assert_eq!(store.read_event_log(SOURCE).unwrap(), source_log);

    assert_eq!(
        send(&state, cx, 3, open_branch(1, "checkpoint-1")).unwrap(),
        CommandResponse::SessionId(Some(branch.clone())),
    );

    let session_id = forked(send(&state, cx, 4, fork_at(1, "checkpoint-3")));
    settled(cx, &session_id);
    state.read(|state| {
        let carried = state.residents.live[&session_id]
            .timeline
            .entries
            .iter()
            .any(|entry| {
                matches!(&entry.content, EntryContent::Rewound(rewound) if rewound.branch.is_some())
            });
        assert!(!carried, "only the thread that has the divider opens it");
    });
}

/// A later send cannot revive a branch a Harness does not keep: Tcode offers
/// the branch of a rewind that left one, never another's.
#[test]
fn a_rewind_that_left_no_branch_opens_nothing() {
    for (later_history, undo) in [
        (agent::LaterHistory::Truncated, None),
        (agent::LaterHistory::HiddenUntilNextSend, Some("revert")),
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-no-branch");
        let cwd = store.root().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let state = host(&store, cx);
        state.update(cx, |state, cx| {
            install(state, ProviderKind::OpenCode, cwd, cx);
            turn(state, cx, 0, "first", &[]);
            turn(state, cx, 1, "second", &[]);
            state.record_event(SOURCE, &rewound("checkpoint-1", later_history, undo), cx);
            turn(state, cx, 2, "again", &[]);
        });
        cx.run_until_parked();
        let refused = send(&state, cx, 1, open_branch(1, "checkpoint-1")).unwrap_err();
        assert_eq!(refused.code, "rewind_unavailable");
    }
}

/// Two rewinds with nothing sent between them: the log of the later branch
/// ends on the earlier divider with its return still open. That return would
/// move the branch's thread into a session that belongs to no thread, so it
/// closes and the earlier branch opens like any other: as one thread, found
/// again however it is asked for. A branch not started yet is not on its
/// session yet, so nothing forks from it.
#[test]
fn a_branch_opened_after_two_rewinds_cannot_return_to_the_earlier_one() {
    const EARLIER: &str = "/sessions/earlier.jsonl";
    const LATER: &str = "/sessions/later.jsonl";
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-two-rewinds");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = host(&store, cx);
    state.update(cx, |state, cx| {
        install(state, ProviderKind::Pi, cwd, cx);
        turn(state, cx, 0, "first", &[]);
        turn(state, cx, 1, "second", &[]);
        turn(state, cx, 2, "third", &[]);
        state.record_event(
            SOURCE,
            &rewound("checkpoint-2", agent::LaterHistory::Branch, Some(EARLIER)),
            cx,
        );
        state.record_event(
            SOURCE,
            &rewound("checkpoint-1", agent::LaterHistory::Branch, Some(LATER)),
            cx,
        );
        turn(state, cx, 3, "second, edited", &[]);
    });
    cx.run_until_parked();

    let CommandResponse::SessionId(Some(branch)) =
        send(&state, cx, 1, open_branch(1, "checkpoint-1")).unwrap()
    else {
        panic!("the branch answers with its thread");
    };
    settled(cx, &branch);
    state.read(|state| {
        assert_eq!(
            state.residents.live[&branch].meta.pending_fork,
            Some(ForkPoint::Branch {
                token: LATER.into()
            })
        );
    });

    let undone = json!({"type": "undo_rewind", "content": {
        "session_id": branch, "checkpoint_id": "checkpoint-2",
    }});
    let refused = send(&state, cx, 2, undone).unwrap_err();
    assert_eq!(refused.code, "rewind_unavailable");

    let refused = send(
        &state,
        cx,
        3,
        json!({"type": "fork_thread", "content": {
            "id": branch, "at": {"turn": 1, "checkpoint_id": "checkpoint-1"},
        }}),
    )
    .unwrap_err();
    assert_eq!(refused.code, "fork_unavailable");

    let open_earlier = json!({"type": "open_rewound_branch", "content": {
        "session_id": branch, "turn": 2, "checkpoint_id": "checkpoint-2",
    }});
    let CommandResponse::SessionId(Some(earlier)) =
        send(&state, cx, 4, open_earlier.clone()).unwrap()
    else {
        panic!("the earlier branch answers with its thread");
    };
    settled(cx, &earlier);
    state.read(|state| {
        assert_eq!(
            state.residents.live[&earlier].meta.pending_fork,
            Some(ForkPoint::Branch {
                token: EARLIER.into()
            })
        );
    });
    assert_eq!(
        send(&state, cx, 5, open_earlier).unwrap(),
        CommandResponse::SessionId(Some(earlier)),
    );
}

/// A branch thread that rewinds and sends goes on in a session of its own,
/// so the branch it was opened on is no longer its: the source's divider and
/// its own both open that branch again, as one thread, which neither asks
/// for twice.
#[test]
fn the_branch_a_branch_thread_left_opens_again_as_a_thread_of_its_own() {
    const LEFT: &str = "/sessions/left.jsonl";
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-branch-left-again");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = host(&store, cx);
    state.update(cx, |state, cx| {
        install(state, ProviderKind::Pi, cwd, cx);
        turn(state, cx, 0, "first", &[]);
        turn(state, cx, 1, "second", &[]);
        state.record_event(
            SOURCE,
            &rewound("checkpoint-1", agent::LaterHistory::Branch, Some(LEFT)),
            cx,
        );
        turn(state, cx, 2, "second, edited", &[]);
    });
    cx.run_until_parked();
    let CommandResponse::SessionId(Some(branch)) =
        send(&state, cx, 1, open_branch(1, "checkpoint-1")).unwrap()
    else {
        panic!("the branch answers with its thread");
    };
    settled(cx, &branch);

    let rewound_in_branch = json!({"type": "open_rewound_branch", "content": {
        "session_id": branch, "turn": 1, "checkpoint_id": "checkpoint-1",
    }});
    state.update(cx, |state, cx| {
        state.record_event(
            &branch,
            &rewound("checkpoint-1", agent::LaterHistory::Branch, Some(LEFT)),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(
        send(&state, cx, 2, open_branch(1, "checkpoint-1")).unwrap(),
        CommandResponse::SessionId(Some(branch.clone())),
        "until it sends, the branch thread can go back to it"
    );

    state.update(cx, |state, cx| {
        for event in [
            AgentEvent::ItemCompleted(ThreadItem {
                id: "branch-user".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "second, again".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            AgentEvent::TurnStarted {
                turn_id: "branch-turn".into(),
            },
            AgentEvent::TurnCompleted {
                turn_id: "branch-turn".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ] {
            state.record_event(&branch, &event, cx);
        }
    });
    cx.run_until_parked();
    let CommandResponse::SessionId(Some(original)) =
        send(&state, cx, 3, open_branch(1, "checkpoint-1")).unwrap()
    else {
        panic!("the original branch answers with its thread");
    };
    assert_ne!(original, branch);
    settled(cx, &original);
    state.read(|state| {
        let thread = &state.residents.live[&original];
        assert_eq!(
            thread.meta.pending_fork,
            Some(ForkPoint::Branch { token: LEFT.into() })
        );
        assert_eq!(prompts(thread), ["first", "second"]);
    });
    assert_eq!(
        send(&state, cx, 4, rewound_in_branch).unwrap(),
        CommandResponse::SessionId(Some(original.clone())),
    );
    assert_eq!(
        send(&state, cx, 5, open_branch(1, "checkpoint-1")).unwrap(),
        CommandResponse::SessionId(Some(original)),
    );
}

/// Work the source left running in the background has no process in a fork:
/// the fork ends it as interrupted, whichever way it was made, and the
/// source's item and log stay as they were.
#[test]
fn background_work_open_at_a_fork_point_ends_as_interrupted_in_the_fork() {
    fn status(session: &ActiveSession) -> (ItemStatus, bool) {
        session
            .timeline
            .entries
            .iter()
            .find_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::CommandExecution { status, .. }) => {
                    Some((*status, entry.finished_ts.is_some()))
                }
                _ => None,
            })
            .unwrap()
    }
    let forks = [
        json!({"type": "fork_thread", "content": {"id": SOURCE}}),
        fork_at(0, "checkpoint-0"),
    ];
    for command in forks {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-fork-background");
        let cwd = store.root().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let state = host(&store, cx);
        state.update(cx, |state, cx| {
            install(state, ProviderKind::ClaudeCode, cwd, cx);
            state.record_event(
                SOURCE,
                &AgentEvent::ItemCompleted(ThreadItem {
                    id: "user-0".into(),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: "first".into(),
                        context_len: None,
                        attachments: Vec::new(),
                    },
                }),
                cx,
            );
            for event in [
                AgentEvent::TurnStarted {
                    turn_id: "turn-0".into(),
                },
                AgentEvent::TurnCheckpoint {
                    turn_id: "turn-0".into(),
                    checkpoint_id: "checkpoint-0".into(),
                },
                AgentEvent::ItemStarted(ThreadItem {
                    id: "serve".into(),
                    parent_item_id: None,
                    content: ItemContent::CommandExecution {
                        command: "serve".into(),
                        output: "listening".into(),
                        exit_code: None,
                        status: ItemStatus::InProgress,
                    },
                }),
                AgentEvent::ItemBackgrounded {
                    item_id: "serve".into(),
                    parent_item_id: None,
                },
                AgentEvent::TurnCompleted {
                    turn_id: "turn-0".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ] {
                state.record_event(SOURCE, &event, cx);
            }
            turn(state, cx, 1, "second", &[]);
        });
        cx.run_until_parked();
        let source_log = store.read_event_log(SOURCE).unwrap();

        let fork = forked(send(&state, cx, 1, command));
        settled(cx, &fork);

        state.read(|state| {
            assert_eq!(
                status(&state.residents.live[&fork]),
                (ItemStatus::Interrupted, true)
            );
            assert_eq!(
                status(&state.residents.live[SOURCE]),
                (ItemStatus::InProgress, false),
                "the source's work goes on"
            );
            assert!(state.residents.live[&fork].meta.pending_fork.is_some());
        });
        assert_eq!(store.read_event_log(SOURCE).unwrap(), source_log);
    }
}
