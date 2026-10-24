use super::*;
use crate::app::active_session::{QueuedMessageKind, Runtime};
use crate::app::test_support::*;
use control_mcp::{
    CancelNotifyParams, DeleteThreadsParams, GetThreadParams, GitOp, GitParams, NotifyWhenParams,
    OutlineParams, PatchSettingsParams, RequestGrantParams, RespondInputParams, TerminalOp,
    TerminalParams, ThreadChange, ThreadOp, UpdateThreadsParams,
};
use tcode_core::control::{ControlCategory, ControlTier, WatchEvent};
use tcode_protocol::{Command, CommandResponse, HostMessage};

fn thread(id: &str, project: &str) -> SessionMeta {
    let mut meta = SessionMeta::new(
        ProviderKind::ClaudeCode,
        PathBuf::from("/tmp/synthetic-checkout"),
        None,
    );
    meta.id = id.into();
    meta.title = format!("Thread {id}");
    meta.project_id = Some(project.into());
    meta
}

fn caller(project: &str, grant: ControlGrant) -> Caller {
    Caller {
        thread_id: "caller".into(),
        project_id: Some(project.into()),
        grant,
    }
}

fn tool_names(state: &AppState, id: &str) -> Vec<String> {
    state
        .session_status_snapshot(id)
        .unwrap()
        .injected_tools
        .map(|tools| tools.tools.into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default()
}

/// A thread whose provider takes MCP servers is registered with `tcode_control` and its
/// grant's tools are disclosed; turning the setting off withdraws both.
#[test]
fn control_tools_follow_the_grant_and_the_setting() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-registration");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let mut host = mcp_host::Host::bind().unwrap();
    let server = control_mcp::start(&mut host);
    let meta = thread("caller", "p");
    state.update(cx, |state, _| {
        state.attach_control_mcp(server);
        state.sessions.push(meta.clone());
        state.install_selected(ActiveSession::new(meta.clone(), false, Vec::new()));
        let registration = state.control_registration_for(&meta).unwrap();
        assert_eq!(
            registration.name,
            agent::McpRegistration::SERVER_NAME_CONTROL
        );
        let tools = tool_names(state, "caller");
        assert!(tools.iter().any(|name| name == "list_projects"));
        assert!(tools.iter().any(|name| name == "request_grant"));
        assert!(!tools.iter().any(|name| name == "delete_threads"));
        assert!(
            state
                .session_status_snapshot("caller")
                .unwrap()
                .control
                .unwrap()
                .tools_offered
        );
    });

    state.dispatch_command(
        cx,
        1,
        Command::PatchSettings {
            patch: SettingsPatch::ControlEnabled(false),
        },
    );
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let meta = state.find_meta("caller").unwrap();
        assert!(state.control_registration_for(&meta).is_none());
        assert!(tool_names(state, "caller").is_empty());
        assert!(
            !state
                .session_status_snapshot("caller")
                .unwrap()
                .control
                .unwrap()
                .tools_offered
        );
    });
}

/// A call outside the grant is refused with the reason, and the operations no grant exposes
/// are refused or absent whatever the grant.
#[test]
fn control_calls_outside_the_grant_are_refused_with_the_reason() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-authorize");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        state.sessions.push(thread("caller", "a"));
        state.sessions.push(thread("same", "a"));
        state.sessions.push(thread("other", "b"));
        let mut referenced = thread("referenced", "b");
        referenced.id = "referenced".into();
        state.sessions.push(referenced);
        let mut child = thread("child", "b");
        child.parent_session_id = Some("caller".into());
        state.sessions.push(child);
    });

    let observe = ControlGrant {
        tiers: [ControlTier::Observe].into_iter().collect(),
        ..ControlGrant::none()
    };
    let settle = ControlOp::UpdateThreads(UpdateThreadsParams {
        changes: vec![ThreadChange {
            thread_id: "same".into(),
            op: ThreadOp::Settle,
            expect: None,
        }],
        dry_run: None,
        request_id: None,
    });
    let delete = ControlOp::DeleteThreads(DeleteThreadsParams {
        thread_ids: vec!["same".into()],
        remove_worktree: None,
        dry_run: None,
        request_id: None,
    });
    let get = |id: &str| {
        ControlOp::GetThread(GetThreadParams {
            thread_id: id.into(),
        })
    };
    state.read(|state| {
        assert_eq!(
            state.authorize_control(&caller("a", observe.clone()), &settle),
            Err("refused: `update_threads` needs tier organize; your grant: observe (project)".into())
        );
        assert_eq!(
            state.authorize_control(&caller("a", ControlGrant::default_grant()), &delete),
            Err("refused: `delete_threads` needs category delete; your grant: observe, organize, operate (project)".into())
        );
        assert_eq!(
            state.authorize_control(&caller("a", ControlGrant::default_grant()), &settle),
            Ok(())
        );
        assert_eq!(
            state.authorize_control(&caller("a", observe.clone()), &get("other")),
            Err("refused: thread other is outside your grant's scope (project)".into())
        );
        assert_eq!(
            state.authorize_control(&caller("a", observe.clone()), &get("missing")),
            Err("unknown thread: missing".into())
        );
        assert_eq!(
            state.authorize_control(&caller("a", observe.clone()), &get("child")),
            Ok(()),
            "an orchestrated child is readable across projects"
        );
        let host = ControlGrant {
            scope: ControlScope::Host,
            ..observe.clone()
        };
        assert_eq!(
            state.authorize_control(&caller("a", host), &get("other")),
            Ok(())
        );
        let push = ControlOp::Git(GitParams {
            thread_id: "same".into(),
            op: GitOp::Push,
            request_id: None,
        });
        assert_eq!(
            state.authorize_control(&caller("a", ControlGrant::default_grant()), &push),
            Err("refused: `git` needs category git_remote; your grant: observe, organize, operate (project)".into())
        );
    });

    state.update(cx, |state, cx| {
        state
            .add_thread_reference("caller", "referenced", cx)
            .unwrap();
        assert_eq!(
            state.authorize_control(
                &caller("a", observe.clone()),
                &ControlOp::Outline(OutlineParams {
                    thread_id: "referenced".into(),
                    level: None,
                    range: None,
                })
            ),
            Ok(()),
            "a referenced thread is readable across projects"
        );
    });

    let every_category = ControlGrant {
        scope: ControlScope::Host,
        tiers: ControlTier::ALL.into_iter().collect(),
        categories: ControlCategory::ALL.into_iter().collect(),
    };
    assert!(
        control_mcp::tool_descriptions(&every_category)
            .iter()
            .all(|(name, _)| !name.contains("approv")),
        "approvals are never a tool"
    );
    state.read(|state| {
        for patch in [
            SettingsPatch::ControlEnabled(false),
            SettingsPatch::ControlDefaultGrant(every_category.clone()),
            SettingsPatch::SettlementAutomation(tcode_core::control::SettlementAutomation::Off),
            SettingsPatch::RemoteHostingEnabled(true),
            SettingsPatch::SourceControlHost {
                host: "github.com".into(),
                kind: tcode_core::pull_request::HostKind::Github,
                enabled: None,
                account: None,
            },
        ] {
            let op = ControlOp::PatchSettings(PatchSettingsParams {
                patch: serde_json::to_value(&patch).unwrap(),
                request_id: None,
            });
            let refusal = state
                .authorize_control(&caller("a", every_category.clone()), &op)
                .unwrap_err();
            assert!(
                refusal.starts_with("refused: `patch_settings` never changes"),
                "{patch:?}: {refusal}"
            );
        }
        let allowed = ControlOp::PatchSettings(PatchSettingsParams {
            patch: serde_json::to_value(SettingsPatch::WordWrapDiffs(true)).unwrap(),
            request_id: None,
        });
        assert_eq!(
            state.authorize_control(&caller("a", every_category.clone()), &allowed),
            Ok(())
        );
    });
}

/// `request_grant` asks; the user's approval widens the thread's effective grant and the
/// thread hears the answer as an agent-origin message.
#[test]
fn an_approved_grant_request_widens_the_grant_and_wakes_the_thread() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-grant-request");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let request_id = state.update(cx, |state, cx| {
        let meta = thread("caller", "p");
        state.sessions.push(meta.clone());
        let mut session = ActiveSession::new(meta, false, Vec::new());
        session.runtime = Runtime::Starting { generation: 1 };
        state.residents.parked.insert("caller".into(), session);

        let (reply, response) = smol::channel::bounded(1);
        state.handle_control_op(
            "caller".into(),
            ControlOp::RequestGrant(RequestGrantParams {
                scope: ControlScope::Host,
                tiers: Vec::new(),
                categories: vec![ControlCategory::Delete],
                reason: "clean up finished threads".into(),
            }),
            reply,
            cx,
        );
        let answer = response.try_recv().unwrap().unwrap();
        assert_eq!(answer["status"], "pending");
        let request_id = answer["request_id"].as_str().unwrap().to_owned();
        let meta = state.find_meta("caller").unwrap();
        assert_eq!(meta.control_grant_requests[0].id, request_id);
        assert_eq!(
            meta.control_grant_requests[0].reason,
            "clean up finished threads"
        );
        assert_eq!(
            state.caller_grant(&meta),
            (ControlGrant::default_grant(), ControlGrantSource::Default)
        );
        request_id
    });
    cx.run_until_parked();
    cx.drain_outgoing();

    state.dispatch_command(
        cx,
        7,
        Command::RespondControlGrant {
            session_id: "caller".into(),
            request_id,
            approve: true,
        },
    );
    cx.run_until_parked();
    assert!(cx.drain_outgoing().iter().any(|message| matches!(
        message,
        HostMessage::Ack {
            id: 7,
            result: Ok(CommandResponse::Unit)
        }
    )));
    state.read(|state| {
        let meta = state.find_meta("caller").unwrap();
        assert!(meta.control_grant_requests.is_empty());
        let (grant, source) = state.caller_grant(&meta);
        assert_eq!(source, ControlGrantSource::Thread);
        assert_eq!(grant.scope, ControlScope::Host);
        assert!(grant.allows_tier(ControlTier::Operate));
        assert!(grant.allows_category(ControlCategory::Delete));
        assert_eq!(
            state
                .session_status_snapshot("caller")
                .unwrap()
                .control
                .unwrap()
                .grant,
            grant
        );

        let session = state.residents.parked.get("caller").unwrap();
        assert_eq!(session.queue.len(), 1);
        assert_eq!(session.queue[0].origin, MessageOrigin::Agent);
        assert_eq!(
            session.queue[0].kind,
            QueuedMessageKind::OrchestrateCallback
        );
        assert!(
            session.queue[0]
                .text
                .starts_with("[control] grant approved: delete (host)"),
            "{}",
            session.queue[0].text
        );
        let audited = session.timeline.entries.iter().any(|entry| {
            matches!(
                &entry.content,
                EntryContent::ControlAction(action)
                    if action.actor_thread_id == "caller" && action.action == "request_grant"
            )
        });
        assert!(audited, "the request is recorded in the thread");
        assert_eq!(state.control_activity(10).len(), 1);
    });
}

fn call(
    state: &mut AppState,
    thread_id: &str,
    op: ControlOp,
    cx: &mut HostCx,
) -> Result<serde_json::Value, String> {
    let (reply, response) = smol::channel::bounded(1);
    state.handle_control_op(thread_id.into(), op, reply, cx);
    response.try_recv().unwrap()
}

fn granted(mut meta: SessionMeta, categories: &[ControlCategory]) -> SessionMeta {
    meta.control_grant = Some(ControlGrant {
        categories: categories.iter().copied().collect(),
        ..ControlGrant::default_grant()
    });
    meta
}

/// A thread whose provider is still starting: a callback waits in its queue.
fn park_waiting(state: &mut AppState, meta: SessionMeta) {
    let mut session = ActiveSession::new(meta.clone(), false, Vec::new());
    session.runtime = Runtime::Starting { generation: 1 };
    state.residents.parked.insert(meta.id.clone(), session);
}

fn install_live(
    state: &mut AppState,
    meta: SessionMeta,
) -> smol::channel::Receiver<agent::SessionCommand> {
    let (commands, receiver) = smol::channel::unbounded();
    let mut session = ActiveSession::new(meta.clone(), false, Vec::new());
    session.runtime = Runtime::Live(commands);
    session.turn_in_flight = true;
    state.residents.live.insert(meta.id.clone(), session);
    receiver
}

fn question(request_id: &str) -> AgentEvent {
    AgentEvent::UserInputRequested {
        request_id: request_id.into(),
        questions: vec![agent::UserInputQuestion {
            id: "which".into(),
            header: "Branch".into(),
            question: "Which branch should I rebase onto?".into(),
            options: Vec::new(),
            multi_select: false,
            prefill: None,
        }],
        delivery: agent::UserInputDelivery::Blocking,
    }
}

fn turn_completed() -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: "turn-1".into(),
        status: agent::TurnStatus::Completed,
        usage: None,
    }
}

fn queued_texts(state: &AppState, id: &str) -> Vec<(String, MessageOrigin)> {
    state
        .resident(id)
        .unwrap()
        .queue
        .iter()
        .map(|message| (message.text.clone(), message.origin))
        .collect()
}

/// A watcher hears each watched event once, as an agent-origin `[control]` message that
/// reads back as a notice, without polling.
#[test]
fn a_watch_wakes_the_watcher_when_the_thread_asks_and_finishes() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-watch");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        let watcher = thread("caller", "p");
        let worker = thread("worker", "p");
        state.sessions.push(watcher.clone());
        state.sessions.push(worker.clone());
        park_waiting(state, watcher);
        let _commands = install_live(state, worker);

        let watch = call(
            state,
            "caller",
            ControlOp::NotifyWhen(NotifyWhenParams {
                thread_ids: vec!["worker".into()],
                events: vec![WatchEvent::NeedsInput, WatchEvent::TurnCompleted],
                once: None,
                request_id: None,
            }),
            cx,
        )
        .unwrap();
        assert!(watch["id"].as_str().is_some_and(|id| !id.is_empty()));

        state.provider_event_for_test("worker", question("q1"), cx);
        state.provider_event_for_test("worker", turn_completed(), cx);
        let expected = "[control] thread worker (\"Thread worker\") needs_input: \"Which branch should I rebase onto?\" (request_id: q1)\n\n[control] thread worker (\"Thread worker\") turn_completed: completed";
        assert_eq!(
            queued_texts(state, "caller"),
            vec![(expected.to_owned(), MessageOrigin::Agent)]
        );
        assert!(matches!(
            tcode_core::notice::host_notice(expected),
            Some(tcode_core::notice::HostNotice::Control(messages)) if messages.len() == 2
        ));
    });
}

/// Watches are stored on the watcher and outlive a host restart; `once` ends a watch
/// when it fires and `cancel_notify` ends the rest.
#[test]
fn watches_survive_a_restart_fire_once_and_cancel() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-watch-restart");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let idle_watch = state.update(cx, |state, cx| {
        state.persist_meta(&thread("caller", "p"), cx);
        state.persist_meta(&thread("worker", "p"), cx);
        let notify = |events: Vec<WatchEvent>, once| {
            ControlOp::NotifyWhen(NotifyWhenParams {
                thread_ids: vec!["worker".into()],
                events,
                once: Some(once),
                request_id: None,
            })
        };
        call(
            state,
            "caller",
            notify(vec![WatchEvent::TurnCompleted], true),
            cx,
        )
        .unwrap();
        call(state, "caller", notify(vec![WatchEvent::Idle], false), cx).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    });
    cx.run_until_parked();

    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        assert_eq!(state.find_meta("caller").unwrap().control_watches.len(), 2);
        let (caller, worker) = (
            state.find_meta("caller").unwrap(),
            state.find_meta("worker").unwrap(),
        );
        park_waiting(state, caller);
        let _commands = install_live(state, worker);

        state.provider_event_for_test("worker", turn_completed(), cx);
        assert_eq!(
            queued_texts(state, "caller"),
            vec![(
                "[control] thread worker (\"Thread worker\") turn_completed: completed\n\n[control] thread worker (\"Thread worker\") idle".to_owned(),
                MessageOrigin::Agent
            )]
        );
        let watches = state.find_meta("caller").unwrap().control_watches;
        assert_eq!(watches.len(), 1, "the once watch is gone");
        assert_eq!(watches[0].id, idle_watch);

        let cancelled = call(
            state,
            "caller",
            ControlOp::CancelNotify(CancelNotifyParams {
                ids: vec![idle_watch.clone(), "missing".into()],
            }),
            cx,
        )
        .unwrap();
        assert_eq!(cancelled["removed"], serde_json::json!([idle_watch]));
        assert_eq!(cancelled["unknown"], serde_json::json!(["missing"]));
        state.residents.parked.get_mut("caller").unwrap().queue.clear();
        state.resident_mut("worker").unwrap().turn_in_flight = true;
        state.provider_event_for_test("worker", turn_completed(), cx);
        assert!(queued_texts(state, "caller").is_empty());
    });
}

/// `respond_input` answers another thread's pending question through the provider, only
/// with `answer_input` and only for the request actually pending.
#[test]
fn respond_input_answers_only_the_pending_question_with_the_category() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-respond-input");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        state.sessions.push(thread("caller", "p"));
        state.sessions.push(thread("asker", "p"));
        let commands = install_live(state, thread("asker", "p"));
        state.provider_event_for_test("asker", question("q1"), cx);
        let answer = |question_id: Option<&str>| {
            ControlOp::RespondInput(RespondInputParams {
                thread_id: "asker".into(),
                question_id: question_id.map(str::to_owned),
                answers: [("which".to_owned(), serde_json::json!("main"))]
                    .into_iter()
                    .collect(),
                request_id: None,
            })
        };

        assert_eq!(
            call(state, "caller", answer(None), cx),
            Err("refused: `respond_input` needs category answer_input; your grant: observe, organize, operate (project)".into())
        );
        let meta = granted(state.find_meta("caller").unwrap(), &[ControlCategory::AnswerInput]);
        state.store_meta(meta, cx);
        assert_eq!(
            call(state, "caller", answer(Some("q0")), cx),
            Err("refused: request q0 is not pending in thread asker".into())
        );
        assert!(commands.try_recv().is_err());

        call(state, "caller", answer(Some("q1")), cx).unwrap();
        match commands.try_recv().unwrap() {
            agent::SessionCommand::RespondUserInput {
                request_id,
                answers,
            } => {
                assert_eq!(request_id, "q1");
                assert_eq!(answers["which"], "main");
            }
            other => panic!("unexpected command: {other:?}"),
        }
        let audited = state.resident("asker").unwrap().timeline.entries.iter().any(|entry| {
            matches!(
                &entry.content,
                EntryContent::ControlAction(action)
                    if action.actor_thread_id == "caller" && action.action == "respond_input"
            )
        });
        assert!(audited, "the answer is recorded in the answered thread");
    });
}

/// A terminal is reachable only through the thread whose panel holds it, so naming an
/// in-scope thread does not reach another project's shell.
#[test]
fn terminal_input_cannot_reach_an_out_of_scope_threads_terminal() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-terminal");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        state
            .sessions
            .push(granted(thread("caller", "a"), &[ControlCategory::Terminal]));
        state.sessions.push(thread("same", "a"));
        let mut other = thread("other", "b");
        other.cwd = std::env::temp_dir();
        state.sessions.push(other.clone());
        let _ = install_live(state, other);
    });
    state.dispatch_command(
        cx,
        1,
        Command::NewTerminal {
            session_id: "other".into(),
        },
    );
    cx.run_until(|state| {
        !state
            .resident("other")
            .unwrap()
            .terminal_workspace
            .terminals
            .is_empty()
    });
    state.update(cx, |state, cx| {
        let terminal_id = state
            .resident("other")
            .unwrap()
            .terminal_workspace
            .terminals[0]
            .id;
        let input = |thread_id: &str| {
            ControlOp::Terminal(TerminalParams {
                thread_id: thread_id.into(),
                op: TerminalOp::Input {
                    terminal_id,
                    text: "echo forbidden\n".into(),
                },
                request_id: None,
            })
        };
        assert_eq!(
            call(state, "caller", input("same"), cx),
            Err(format!(
                "refused: terminal {terminal_id} is not one of thread same's terminals"
            ))
        );
        assert_eq!(
            call(state, "caller", input("other"), cx),
            Err("refused: thread other is outside your grant's scope (project)".into())
        );
        state.close_terminal("other", terminal_id, cx);
    });
}

/// A dry run of `delete_threads` changes nothing; the real run deletes the thread with
/// every thread under it and records it on the caller.
#[test]
fn delete_threads_previews_then_deletes_the_tree() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-delete");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        let caller = granted(thread("caller", "p"), &[ControlCategory::Delete]);
        state.sessions.push(caller.clone());
        park_waiting(state, caller);
        state.sessions.push(thread("lead", "p"));
        let mut kid = thread("kid", "p");
        kid.parent_session_id = Some("lead".into());
        state.sessions.push(kid);
        let delete = |dry_run| {
            ControlOp::DeleteThreads(DeleteThreadsParams {
                thread_ids: vec!["lead".into()],
                remove_worktree: None,
                dry_run: Some(dry_run),
                request_id: None,
            })
        };

        let preview = call(state, "caller", delete(true), cx).unwrap();
        assert_eq!(preview["results"][0]["outcome"], "would_apply");
        assert_eq!(
            preview["results"][0]["side_effects"][0],
            "deletes \"Thread lead\" and 1 thread(s) under it, permanently"
        );
        assert!(state.find_meta("lead").is_some() && state.find_meta("kid").is_some());

        let deleted = call(state, "caller", delete(false), cx).unwrap();
        assert_eq!(deleted["results"][0]["outcome"], "applied");
        assert!(state.find_meta("lead").is_none() && state.find_meta("kid").is_none());
        let audited = state
            .resident("caller")
            .unwrap()
            .timeline
            .entries
            .iter()
            .any(|entry| {
                matches!(
                    &entry.content,
                    EntryContent::ControlAction(action) if action.action == "delete_threads"
                )
            });
        assert!(audited, "the deletion is recorded on the caller");
    });
}

/// `patch_settings` applies an ordinary setting and refuses the ones that change
/// authority or credentials, whatever the grant.
#[test]
fn patch_settings_applies_allowed_patches_only() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-patch-settings");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        state.sessions.push(granted(
            thread("caller", "p"),
            &[ControlCategory::SettingsWrite],
        ));
        let patch = |patch: SettingsPatch| {
            ControlOp::PatchSettings(PatchSettingsParams {
                patch: serde_json::to_value(patch).unwrap(),
                request_id: None,
            })
        };
        for refused in [
            SettingsPatch::ControlEnabled(false),
            SettingsPatch::SourceControlHost {
                host: "github.com".into(),
                kind: tcode_core::pull_request::HostKind::Github,
                enabled: None,
                account: None,
            },
        ] {
            let refusal = call(state, "caller", patch(refused), cx).unwrap_err();
            assert!(refusal.starts_with("refused: "), "{refusal}");
        }
        assert!(state.settings.control.enabled);

        let wrap = !state.settings.word_wrap_diffs;
        call(
            state,
            "caller",
            patch(SettingsPatch::WordWrapDiffs(wrap)),
            cx,
        )
        .unwrap();
        assert_eq!(state.settings.word_wrap_diffs, wrap);
    });
}
