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

/// Sends one command and runs the host until it answers.
fn acked(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    command: Command,
) -> (
    Result<tcode_protocol::CommandResponse, tcode_protocol::ProtocolError>,
    Vec<HostMessage>,
) {
    state.dispatch_command(cx, id, command);
    cx.run_until_sent(|message| match message {
        HostMessage::Ack { id: acked, result } if *acked == id => Some(result.clone()),
        _ => None,
    })
}
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
        state.set_provider_launcher(scripted.launcher);
        state
    });
    state.update(cx, |state, cx| {
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
    assert!(matches!(
        cx.run_until_command(&commands),
        SessionCommand::Rewind { checkpoint_id, mode: RewindMode::Conversation }
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

    cx.run_until_parked();
    cx.clock.advance(RESIDENT_IDLE_GRACE);
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

/// A rewind the harness refuses is written into the thread and changes
/// nothing else; a message sent while a rewind is made waits for it and goes
/// to the conversation it leaves; and a client that saw another restore
/// point at a turn index the cut reused is refused rather than rewinding the
/// prompt that took its place.
#[test]
fn a_send_waits_for_the_rewind_and_a_stale_target_or_refusal_changes_nothing() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-rewind-sends");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (sender, commands) = smol::channel::unbounded();
    state.update(cx, |state, cx| {
        install(state, ProviderKind::ClaudeCode, cwd, Runtime::Live(sender));
        checkpoint_two_turns(state, cx);
    });
    let status = |state: &TestEntity| {
        state.read(|state| state.session_status_snapshot("rewound-thread").unwrap())
    };

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
    });
    cx.run_until_parked();
    assert!(state.read(|state| {
        state.residents.live["rewound-thread"]
            .timeline
            .entries
            .iter()
            .any(|entry| {
                matches!(
                    &entry.content,
                    EntryContent::RewindFailed { failure, .. }
                        if failure.reason == RewindFailureReason::ConversationChanged
                )
            })
    }));
    assert!(status(&state).rewind.unwrap().pending.is_none());

    state.dispatch_command(cx, 2, rewind_command("desktop"));
    cx.run_until_parked();
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::Rewind { .. })
    ));
    state.dispatch_command(
        cx,
        3,
        Command::SendTurn {
            session_id: "rewound-thread".into(),
            text: "sent while it rewinds".into(),
            attachment_paths: Vec::new(),
        },
    );
    cx.run_until_parked();
    assert!(next_command(&commands).is_err(), "the send waits");
    state.update(cx, |state, cx| {
        state.on_event("rewound-thread", completed("the second prompt"), cx);
    });
    cx.run_until_parked();
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::SendTurn { text, .. }) if text.contains("sent while it rewinds")
    ));
    assert!(!status(&state).queue_held);

    // The cut reused turn 1 for what follows; a client still showing the old
    // prompt there names its restore point and is refused.
    state.update(cx, |state, cx| {
        state.record_event(
            "rewound-thread",
            &AgentEvent::TurnCheckpoint {
                turn_id: "turn-3".into(),
                checkpoint_id: "checkpoint-3".into(),
            },
            cx,
        );
    });
    let (result, _) = acked(
        &state,
        cx,
        4,
        serde_json::from_value(json!({
            "type": "rewind_turn",
            "content": {
                "session_id": "rewound-thread",
                "turn": 1,
                "checkpoint_id": "checkpoint-2",
                "mode": "conversation",
            }
        }))
        .unwrap(),
    );
    assert_eq!(result.unwrap_err().code, "rewind_unavailable");
    assert!(next_command(&commands).is_err());
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
            at: None,
        },
    );
    fn fork(state: &TestClientState) -> Option<&tcode_core::session::Timeline> {
        state
            .residents
            .live
            .iter()
            .find(|(id, _)| *id != "rewound-thread")
            .map(|(_, fork)| &fork.timeline)
    }
    // The fork opens empty and carries the turns once its log, read off the
    // host, is folded.
    cx.run_until(|state| fork(state).is_some_and(|fork| !fork.turns.is_empty()));
    cx.run_until_parked();
    state.read(|state| {
        assert!(
            fork(state)
                .unwrap()
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
    // The answer goes out from a task off the host that waits for the harness's.
    let outcome = cx.run_until_recv(&answer).unwrap();
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

    // The user's next message goes first, and the held one follows it.
    state.dispatch_command(
        cx,
        3,
        Command::SendTurn {
            session_id: "rewound-thread".into(),
            text: "the next message".into(),
            attachment_paths: Vec::new(),
        },
    );
    cx.run_until_parked();
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::SendTurn { text, .. }) if text.contains("the next message")
    ));
    let status = state.read(|state| state.session_status_snapshot("rewound-thread").unwrap());
    assert!(!status.queue_held);
    assert_eq!(
        status
            .queued_messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        ["the next message", "queued behind the turn"]
    );
}

/// A Harness that answers a conversation rewind without the prompt (Codex's
/// `thread/revert`) still gives the asking client its prompt back: the one
/// Tcode stored for the turn, as Codex's own client refills from its
/// transcript.
#[test]
fn a_rewind_answered_without_the_prompt_refills_the_one_tcode_stored() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-rewind-stored-prompt");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (sender, commands) = smol::channel::unbounded();
    state.update(cx, |state, cx| {
        install(state, ProviderKind::Codex, cwd, Runtime::Live(sender));
        for (index, prompt) in [(1, "the first prompt"), (2, "the second prompt")] {
            state.record_user_message("rewound-thread", prompt, &[], None, &[], cx);
            for event in [
                AgentEvent::TurnStarted {
                    turn_id: format!("turn-{index}"),
                },
                AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{index}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
                AgentEvent::TurnCheckpoint {
                    turn_id: format!("turn-{index}"),
                    checkpoint_id: format!("checkpoint-{index}"),
                },
            ] {
                state.record_event("rewound-thread", &event, cx);
            }
        }
    });
    state.dispatch_command(cx, 1, rewind_command("phone"));
    cx.run_until_parked();
    assert!(matches!(
        next_command(&commands),
        Ok(SessionCommand::Rewind { checkpoint_id, .. }) if checkpoint_id == "checkpoint-2"
    ));
    cx.drain_outgoing();
    state.update(cx, |state, cx| {
        let mut answer = completed("unused");
        if let AgentEvent::RewindCompleted { prefill, .. } = &mut answer {
            *prefill = None;
        }
        state.on_event("rewound-thread", answer, cx);
    });
    cx.run_until_parked();
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
}

/// A client folds only the window of the log it was sent. When that window
/// began after a rewind's target but before the rewind, the client's fold
/// could not find where to cut and showed the removed turns as the
/// conversation, with turn numbers that no longer matched the host's.
#[test]
fn a_history_window_never_begins_inside_turns_a_rewind_took_out() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-rewind-window");
    let cwd = store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let (sender, _commands) = smol::channel::unbounded();
    let user = |id: &str| {
        AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: id.into(),
                attachments: Vec::new(),
            },
        })
    };
    state.update(cx, |state, cx| {
        install(state, ProviderKind::ClaudeCode, cwd, Runtime::Live(sender));
        for index in 1..=3 {
            let turn_id = format!("turn-{index}");
            state.record_event("rewound-thread", &user(&format!("prompt-{index}")), cx);
            for event in [
                AgentEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                },
                AgentEvent::TurnCheckpoint {
                    turn_id: turn_id.clone(),
                    checkpoint_id: format!("checkpoint-{index}"),
                },
            ] {
                state.record_event("rewound-thread", &event, cx);
            }
            // The last turn is long: the baseline window reaches back into
            // it, past the turn the rewind goes to.
            if index == 3 {
                for line in 0..500 {
                    state.record_event(
                        "rewound-thread",
                        &AgentEvent::Warning {
                            message: format!("line {line}"),
                        },
                        cx,
                    );
                }
            }
            state.record_event(
                "rewound-thread",
                &AgentEvent::TurnCompleted {
                    turn_id,
                    status: TurnStatus::Completed,
                    usage: None,
                },
                cx,
            );
        }
        let mut rewind = completed("prompt-2");
        if let AgentEvent::RewindCompleted { checkpoint_id, .. } = &mut rewind {
            *checkpoint_id = "checkpoint-2".into();
        }
        state.record_event("rewound-thread", &rewind, cx);
    });
    // The host holds the thread's log once it is read off the host; a
    // subscription made before then is answered with the log as it was read,
    // and the records after it follow the window.
    cx.run_until(|state| state.event_records.contains_key("rewound-thread"));
    cx.drain_outgoing();
    state.deliver(
        cx,
        7,
        ClientPayload::Subscribe(Subscription {
            topic: Topic::SessionEvents {
                session_id: "rewound-thread".into(),
            },
            after: None,
            known_index: None,
        }),
    );
    let records = cx.run_until_sent(|message| match message {
        HostMessage::Event(EventEnvelope {
            request_id: Some(7),
            event: ServerEvent::SessionSnapshot { records, .. },
            ..
        }) => Some(records.clone()),
        _ => None,
    });
    let window = tcode_core::session::Timeline::fold_stored(&records.0);
    let live: Vec<&str> = window
        .entries
        .iter()
        .map(|entry| entry.id.as_str())
        .filter(|id| id.starts_with("prompt-"))
        .collect();
    assert_eq!(
        live,
        ["prompt-1"],
        "the removed prompts are not the conversation"
    );
}
