use std::time::Duration;

use agent::{
    AgentEvent, ProviderKind, RewindFailure, RewindFailureReason, RewindMode, SessionCommand,
    TurnStatus,
};
use serde_json::json;
use tcode_core::project::SessionMeta;
use tcode_core::session::EntryContent;
use tcode_protocol::{
    ClientPayload, Command, EventEnvelope, HostMessage, ServerEvent, Subscription, Topic,
};

use super::*;
use crate::app::test_support::*;
use crate::app::{Runtime, scripted_provider};

/// A thread of `provider`, installed and selected.
fn install(state: &mut AppState, provider: ProviderKind, cwd: PathBuf, runtime: Runtime) {
    let mut session = ActiveSession::new(SessionMeta::new(provider, cwd, None), false, Vec::new());
    session.meta.id = "rewound-thread".into();
    session.meta.resume_cursor = Some(agent::ResumeCursor(json!({ "session_id": "native" })));
    session.runtime = runtime;
    state.sessions.push(session.meta.clone());
    state
        .residents
        .live
        .insert(session.meta.id.clone(), session);
}

/// Two turns its harness checkpointed, as the provider reports them.
fn checkpoint_two_turns(state: &mut AppState, cx: &mut HostCx) {
    for index in 1..=2 {
        for event in [
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
        ] {
            state.record_event("rewound-thread", &event, cx);
        }
        if let Some(session) = state.resident_mut("rewound-thread") {
            session.turn_in_flight = false;
        }
    }
}

/// The command as a client sends it.
fn rewind_command(client_id: &str) -> Command {
    serde_json::from_value(json!({
        "type": "rewind_turn",
        "content": {
            "session_id": "rewound-thread",
            "turn": 1,
            "mode": "conversation",
            "client_id": client_id,
        }
    }))
    .unwrap()
}

fn completed(prefill: &str) -> AgentEvent {
    AgentEvent::RewindCompleted {
        checkpoint_id: "checkpoint-2".into(),
        mode: RewindMode::Conversation,
        prefill: Some(prefill.into()),
        prefill_attachments: Vec::new(),
        later_history: agent::LaterHistory::Truncated,
        files: None,
        incomplete: None,
        undo_token: None,
    }
}

fn divider_count(state: &AppState) -> usize {
    state.residents.live["rewound-thread"]
        .timeline
        .entries
        .iter()
        .filter(|entry| matches!(entry.content, EntryContent::Rewound(_)))
        .count()
}

/// A rewind on a thread whose provider is not running starts it, and once
/// the harness answers, the restored prompt goes to the client that asked
/// and the provider is let go after the idle grace like after any turn.
#[test]
fn a_cold_rewind_answers_the_client_that_asked_and_lets_the_provider_go() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-cold-rewind");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let commands = scripted.commands.clone();
    let events = scripted.events.clone();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*store).clone());
        state.set_provider_launcher_for_test(scripted.launcher);
        state
    });
    state.update(cx, |state, cx| {
        state.resident_idle_grace = Duration::from_millis(1);
        install(state, ProviderKind::ClaudeCode, cwd, Runtime::Idle);
        checkpoint_two_turns(state, cx);
    });
    state.deliver(
        cx,
        1,
        ClientPayload::Subscribe(Subscription {
            topic: Topic::SessionStatus {
                session_id: "rewound-thread".into(),
            },
            after: None,
            known_index: None,
        }),
    );
    state.dispatch_command(cx, 2, rewind_command("phone"));
    cx.run_until(|_| !commands.is_empty());
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::Rewind { checkpoint_id, mode: RewindMode::Conversation })
            if checkpoint_id == "checkpoint-2"
    ));
    assert!(
        state
            .read(|state| state.session_status_snapshot("rewound-thread").unwrap())
            .rewind
            .unwrap()
            .pending
            .is_some(),
        "the thread shows the rewind in progress"
    );

    events.try_send(completed("the second prompt")).unwrap();
    cx.run_until(|state| divider_count(state) == 1);
    let prefills: Vec<_> = cx
        .drain_outgoing()
        .into_iter()
        .filter_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                event:
                    ServerEvent::RewindPrefill {
                        client_id, text, ..
                    },
                ..
            }) => Some((client_id, text)),
            _ => None,
        })
        .collect();
    assert_eq!(prefills, [("phone".into(), "the second prompt".into())]);

    cx.run_until(|state| {
        matches!(
            state.residents.live["rewound-thread"].runtime,
            Runtime::Idle
        )
    });
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::Shutdown)
    ));
}

/// Messages waiting when a rewind is made stay queued until the user sends
/// the next one, which goes before them; a rewind the harness refuses is
/// written into the thread and leaves the queue going.
#[test]
fn a_rewind_holds_the_queue_until_the_next_send_and_a_refusal_is_recorded() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-rewind-queue");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (sender, commands) = smol::channel::unbounded();
    state.update(cx, |state, cx| {
        install(state, ProviderKind::ClaudeCode, cwd, Runtime::Live(sender));
        checkpoint_two_turns(state, cx);
    });

    // A refusal changes nothing; the queue goes on as before.
    state.dispatch_command(cx, 1, rewind_command("desktop"));
    cx.run_until_parked();
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::Rewind { .. })
    ));
    state.update(cx, |state, cx| {
        state.on_event(
            "rewound-thread",
            AgentEvent::RewindFailed {
                checkpoint_id: "checkpoint-2".into(),
                mode: RewindMode::Conversation,
                failure: RewindFailure::new(
                    RewindFailureReason::ConversationChanged,
                    "stale_target".to_owned(),
                ),
            },
            cx,
        );
        let session = &state.residents.live["rewound-thread"];
        assert!(!session.queue_held);
        assert!(session.timeline.entries.iter().any(|entry| matches!(
            &entry.content,
            EntryContent::RewindFailed { failure, .. }
                if failure.reason == RewindFailureReason::ConversationChanged
        )));
    });

    state.dispatch_command(cx, 2, rewind_command("desktop"));
    cx.run_until_parked();
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::Rewind { .. })
    ));
    // Another thread's agent queues a message while the rewind is made.
    state.update(cx, |state, cx| {
        state.queue_automatic_turn(
            "rewound-thread",
            |session| {
                session.push_queued("from an agent".into(), Vec::new());
            },
            cx,
        );
        state.on_event("rewound-thread", completed("the second prompt"), cx);
    });
    cx.run_until_parked();
    assert!(next_command(&commands).is_err(), "the queue waits");
    assert!(
        state
            .read(|state| state.session_status_snapshot("rewound-thread").unwrap())
            .queue_held
    );

    state.dispatch_command(
        cx,
        3,
        Command::SendTurn {
            session_id: "rewound-thread".into(),
            text: "from the user".into(),
            attachment_paths: Vec::new(),
        },
    );
    cx.run_until_parked();
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::SendTurn { text, .. }) if text.contains("from the user")
    ));
    let queue: Vec<_> = state.read(|state| {
        state.residents.live["rewound-thread"]
            .queue
            .iter()
            .map(|message| message.text.clone())
            .collect()
    });
    assert_eq!(queue.last().map(String::as_str), Some("from an agent"));
}

/// A fork's harness session is a new one, so none of the restore points it
/// copied from its parent's log is offered in it.
#[test]
fn a_fork_offers_none_of_its_parents_restore_points() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-rewind-fork");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (sender, _commands) = smol::channel::unbounded();
    state.update(cx, |state, cx| {
        install(state, ProviderKind::ClaudeCode, cwd, Runtime::Live(sender));
        let meta = state.sessions[0].clone();
        state.persist_meta(&meta, cx);
        checkpoint_two_turns(state, cx);
    });
    state.dispatch_command(
        cx,
        1,
        Command::ForkThread {
            id: "rewound-thread".into(),
        },
    );
    cx.run_until(|state| state.residents.live.keys().any(|id| id != "rewound-thread"));
    cx.run_until_parked();
    state.read(|state| {
        let (_, fork) = state
            .residents
            .live
            .iter()
            .find(|(id, _)| *id != "rewound-thread")
            .unwrap();
        assert!(
            !fork.timeline.turns.is_empty(),
            "the fork carries the turns"
        );
        assert!(
            fork.timeline
                .turns
                .iter()
                .all(|turn| turn.provider_checkpoint_id.is_none())
        );
    });
}

/// `tcode_control`'s rewind answers with what the harness did, once it has
/// done it, not with the request alone.
#[test]
fn a_control_rewind_answers_with_the_harness_outcome() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-rewind");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (sender, commands) = smol::channel::unbounded();
    let (reply, answer) = smol::channel::bounded(1);
    state.update(cx, |state, cx| {
        install(
            state,
            ProviderKind::ClaudeCode,
            cwd.clone(),
            Runtime::Live(sender),
        );
        state.sessions[0].project_id = Some("project".into());
        checkpoint_two_turns(state, cx);
        let mut caller = SessionMeta::new(ProviderKind::ClaudeCode, cwd, None);
        caller.id = "caller".into();
        caller.project_id = Some("project".into());
        caller.control_grant = Some(tcode_core::control::ControlGrant {
            categories: [tcode_core::control::ControlCategory::HistoryEdit]
                .into_iter()
                .collect(),
            ..tcode_core::control::ControlGrant::default_grant()
        });
        state.sessions.push(caller);
        state.handle_control_op(
            "caller".into(),
            control_mcp::ControlOp::Rewind(control_mcp::RewindParams {
                thread_id: "rewound-thread".into(),
                turn: 1,
                mode: control_mcp::RewindScope::Conversation,
                request_id: None,
            }),
            reply,
            cx,
        );
    });
    cx.run_until_parked();
    assert!(answer.is_empty(), "no answer before the harness's");
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::Rewind { .. })
    ));
    state.update(cx, |state, cx| {
        state.on_event(
            "rewound-thread",
            AgentEvent::RewindFailed {
                checkpoint_id: "checkpoint-2".into(),
                mode: RewindMode::Conversation,
                failure: RewindFailure::new(RewindFailureReason::TurnRunning, None),
            },
            cx,
        );
    });
    cx.run_until_parked();
    let outcome = answer.try_recv().unwrap().unwrap();
    assert_eq!(outcome["status"], "failed");
    assert_eq!(outcome["reason"], "turn_running");
}

/// Where the harness stops a running turn for a rewind, a rewind can be
/// asked for while one runs; the message queued behind that turn is not sent
/// when the turn stops, and waits for the user's next message.
#[test]
fn a_rewind_during_a_running_turn_keeps_the_queued_message_waiting() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-rewind-running-turn");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (sender, commands) = smol::channel::unbounded();
    state.update(cx, |state, cx| {
        install(state, ProviderKind::ClaudeCode, cwd, Runtime::Live(sender));
        checkpoint_two_turns(state, cx);
        state.on_event(
            "rewound-thread",
            AgentEvent::TurnStarted {
                turn_id: "turn-3".into(),
            },
            cx,
        );
    });
    state.dispatch_command(
        cx,
        1,
        Command::SendTurn {
            session_id: "rewound-thread".into(),
            text: "queued behind the turn".into(),
            attachment_paths: Vec::new(),
        },
    );
    state.dispatch_command(cx, 2, rewind_command("desktop"));
    cx.run_until_parked();
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::Rewind { checkpoint_id, .. }) if checkpoint_id == "checkpoint-2"
    ));

    // Claude stops the turn, then rewinds.
    state.update(cx, |state, cx| {
        state.on_event(
            "rewound-thread",
            AgentEvent::TurnCompleted {
                turn_id: "turn-3".into(),
                status: TurnStatus::Interrupted,
                usage: None,
            },
            cx,
        );
        state.on_event("rewound-thread", completed("the second prompt"), cx);
    });
    cx.run_until_parked();
    assert!(next_command(&commands).is_err(), "the queued message waits");
    let status = state.read(|state| state.session_status_snapshot("rewound-thread").unwrap());
    assert!(status.queue_held);
    assert_eq!(status.queued_messages.len(), 1);
}
