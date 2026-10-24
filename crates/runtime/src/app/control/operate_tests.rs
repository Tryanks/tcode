use super::*;
use crate::app::test_support::*;
use crate::app::{ProviderLauncher, active_session::Runtime};
use control_mcp::{ControlOp, ReferenceParams};
use tcode_core::control::{ControlScope, ControlTier, ReferenceOrigin};

/// Providers that never come up, so what a call queued stays observable.
fn idle_launcher() -> ProviderLauncher {
    ProviderLauncher(Arc::new(|_, _| Box::pin(std::future::pending())))
}

fn host(store: &TestStore, cx: &mut TestAppContext) -> TestEntity {
    let mut state = TestClientState::new((**store).clone());
    state.set_provider_launcher_for_test(idle_launcher());
    cx.new_entity(state)
}

fn call(
    state: &mut AppState,
    cx: &mut HostCx,
    caller: &str,
    op: ControlOp,
) -> Result<serde_json::Value, String> {
    let (reply, response) = smol::channel::bounded(1);
    state.handle_control_op(caller.into(), op, reply, cx);
    response.try_recv().unwrap()
}

fn thread(id: &str, project: &str, provider: ProviderKind) -> SessionMeta {
    let mut meta = SessionMeta::new(provider, PathBuf::from("/tmp/synthetic-checkout"), None);
    meta.id = id.into();
    meta.title = format!("Thread {id}");
    meta.project_id = Some(project.into());
    meta
}

fn park(state: &mut AppState, session: ActiveSession) {
    state.sessions.push(session.meta.clone());
    state
        .residents
        .parked
        .insert(session.meta.id.clone(), session);
}

fn start(project_id: &str) -> ControlOp {
    ControlOp::StartThread(StartThreadParams {
        project_id: project_id.into(),
        cwd: None,
        workspace: None,
        provider: "codex".into(),
        model: None,
        profile: None,
        options: None,
        title: Some("Investigate the flaky test".into()),
        message: "Find out why the sync test flakes".into(),
        attachments: None,
        request_id: "start-1".into(),
    })
}

fn started(state: &AppState) -> Vec<SessionMeta> {
    state
        .sessions
        .iter()
        .filter(|meta| meta.started_by.is_some())
        .cloned()
        .collect()
}

/// `start_thread` makes one ordinary thread whose first message is the agent's, leaves the
/// user's standing draft alone, and a retry with the same request id — also after the host
/// restarts — answers with that thread instead of making another.
#[test]
fn start_thread_makes_one_ordinary_thread_across_retries_and_restarts() {
    let store = TestStore::new("tcode-control-start-thread");
    let root = store.root().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    let project_id = state.update(cx, |state, cx| {
        let project_id = state.create_project(root.clone(), cx).unwrap();
        let mut caller = thread("caller", &project_id, ProviderKind::ClaudeCode);
        caller.cwd = root.clone();
        state.persist_meta(&caller, cx);
        state.start_draft(project_id.clone(), root.clone(), cx);
        project_id
    });
    let draft = state.read(|state| state.active_session_id().unwrap().to_owned());

    let first = state
        .update(cx, |state, cx| {
            call(state, cx, "caller", start(&project_id))
        })
        .unwrap();
    assert_eq!(first["created"], true);
    let id = first["thread_id"].as_str().unwrap().to_owned();
    state.read(|state| {
        let threads = started(state);
        assert_eq!(threads.len(), 1);
        let meta = &threads[0];
        assert_eq!(meta.id, id);
        assert_eq!(
            meta.parent_session_id, None,
            "an ordinary thread, not a child"
        );
        assert_eq!(meta.title, "Investigate the flaky test");
        let session = state.resident(&id).unwrap();
        assert!(!session.draft);
        assert_eq!(session.queue[0].text, "Find out why the sync test flakes");
        assert_eq!(session.queue[0].origin, MessageOrigin::Agent);

        assert_eq!(state.active_session_id(), Some(draft.as_str()));
        let standing = state.resident(&draft).unwrap();
        assert!(standing.draft, "the user's draft stays a draft");
        assert!(standing.queue.is_empty());
    });

    let retry = state
        .update(cx, |state, cx| {
            call(state, cx, "caller", start(&project_id))
        })
        .unwrap();
    assert_eq!(retry["created"], false);
    assert_eq!(retry["thread_id"], id.as_str());
    state.read(|state| assert_eq!(started(state).len(), 1));
    cx.run_until_parked();
    drop(state);

    let cx = &mut TestAppContext::default();
    let restarted = host(&store, cx);
    let after_restart = restarted
        .update(cx, |state, cx| {
            call(state, cx, "caller", start(&project_id))
        })
        .unwrap();
    assert_eq!(after_restart["created"], false);
    assert_eq!(after_restart["thread_id"], id.as_str());
    restarted.read(|state| assert_eq!(started(state).len(), 1));
}

/// `send_message` reaches threads in each mode as the agent's message, reopens a settled
/// thread and says so, and never counts as the user's activity.
#[test]
fn send_message_queues_steers_and_schedules_as_the_agent() {
    let store = TestStore::new("tcode-control-send");
    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    let (commands, steered) = smol::channel::unbounded();
    state.update(cx, |state, _| {
        state
            .sessions
            .push(thread("caller", "p", ProviderKind::ClaudeCode));
        let mut settled = thread("settled", "p", ProviderKind::Codex);
        settled.settled_at = Some(1);
        park(state, ActiveSession::new(settled, false, Vec::new()));
        let mut running = ActiveSession::new(
            thread("running", "p", ProviderKind::Codex),
            false,
            Vec::new(),
        );
        running.runtime = Runtime::Live(commands);
        running.turn_in_flight = true;
        park(state, running);
        park(
            state,
            ActiveSession::new(thread("later", "p", ProviderKind::Codex), false, Vec::new()),
        );
        state.sessions.push(thread("far", "q", ProviderKind::Codex));
    });
    let send = |ids: &[&str], mode: SendMode| {
        ControlOp::SendMessage(SendMessageParams {
            thread_ids: ids.iter().map(|id| id.to_string()).collect(),
            text: "Rebase on main when you can".into(),
            mode,
            attachments: None,
            request_id: None,
        })
    };
    let at = now_secs() + 3600;
    let (queued, steer, scheduled) = state.update(cx, |state, cx| {
        (
            call(state, cx, "caller", send(&["settled"], SendMode::Queue)).unwrap(),
            call(
                state,
                cx,
                "caller",
                send(&["running", "settled", "far"], SendMode::Steer),
            )
            .unwrap(),
            call(
                state,
                cx,
                "caller",
                send(&["later"], SendMode::Schedule { at }),
            )
            .unwrap(),
        )
    });
    cx.run_until_parked();

    assert_eq!(queued["results"][0]["outcome"], "applied");
    assert_eq!(queued["results"][0]["reason"], "queued");
    assert_eq!(
        queued["results"][0]["side_effects"][0],
        "unsettled: a message reopens a settled thread"
    );
    assert_eq!(steer["results"][0]["reason"], "steered");
    assert_eq!(steer["results"][1]["reason"], "queued");
    assert_eq!(
        steer["results"][1]["side_effects"][0],
        "no turn was running; queued instead"
    );
    assert_eq!(
        steer["results"][2],
        serde_json::json!({
            "target": "far",
            "outcome": "skipped",
            "reason": "outside_scope: your grant's scope is project",
        })
    );
    assert_eq!(scheduled["results"][0]["reason"], "scheduled");
    assert!(matches!(
        steered.try_recv(),
        Ok(SessionCommand::Steer { text, .. }) if text == "Rebase on main when you can"
    ));
    state.read(|state| {
        assert!(!state.find_meta("settled").unwrap().is_settled());
        let settled = state.resident("settled").unwrap();
        assert_eq!(settled.queue.len(), 2);
        assert!(
            settled
                .queue
                .iter()
                .all(|m| m.origin == MessageOrigin::Agent)
        );
        let running = state.resident("running").unwrap();
        assert_eq!(running.pending_steers[0].origin, Some(MessageOrigin::Agent));
        let later = state.resident("later").unwrap();
        assert_eq!(later.queue[0].origin, MessageOrigin::Agent);
        assert!(later.queue[0].not_before.is_some());
        for id in ["settled", "running", "later"] {
            assert_eq!(
                state
                    .thread_activity
                    .get(id)
                    .and_then(|activity| activity.last_human_message_at),
                None,
                "{id}: an agent's message is not the user's activity"
            );
        }
    });
}

/// Dropping a queued message the user wrote needs `history_edit`; the agent's own may go.
/// A fork answers with the new thread's id.
#[test]
fn control_thread_guards_human_queued_messages_and_forks() {
    let store = TestStore::new("tcode-control-thread");
    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    let (human, agent) = state.update(cx, |state, _| {
        state
            .sessions
            .push(thread("caller", "p", ProviderKind::ClaudeCode));
        let mut queued = ActiveSession::new(
            thread("queued", "p", ProviderKind::Codex),
            false,
            Vec::new(),
        );
        let human = queued.push_queued("from the user".into(), Vec::new());
        let agent = queued.push_queued("from an agent".into(), Vec::new());
        queued.queue[1].origin = MessageOrigin::Agent;
        park(state, queued);
        let mut source = thread("source", "p", ProviderKind::Codex);
        source.resume_cursor = Some(agent::ResumeCursor(serde_json::json!({"thread_id": "t"})));
        state.sessions.push(source);
        (human, agent)
    });
    let change = |thread_id: &str, op: RunOp| {
        ControlOp::ControlThread(ControlThreadParams {
            changes: vec![RunChange {
                thread_id: thread_id.into(),
                op,
            }],
            request_id: None,
        })
    };
    let (refused, dropped) = state.update(cx, |state, cx| {
        (
            call(
                state,
                cx,
                "caller",
                change("queued", RunOp::DropQueued { id: human }),
            )
            .unwrap(),
            call(
                state,
                cx,
                "caller",
                change("queued", RunOp::DropQueued { id: agent }),
            )
            .unwrap(),
        )
    });
    assert_eq!(refused["results"][0]["outcome"], "failed");
    assert_eq!(
        refused["results"][0]["reason"],
        "refused: dropping a queued human message needs history_edit"
    );
    assert_eq!(dropped["results"][0]["outcome"], "applied");
    state.read(|state| {
        let queue = &state.resident("queued").unwrap().queue;
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id, human);
    });

    state.update(cx, |state, cx| {
        let mut caller = state.find_meta("caller").unwrap();
        let mut grant = ControlGrant::default_grant();
        grant.categories.insert(ControlCategory::HistoryEdit);
        caller.control_grant = Some(grant);
        state.store_meta(caller, cx);
        let allowed = call(
            state,
            cx,
            "caller",
            change("queued", RunOp::DropQueued { id: human }),
        )
        .unwrap();
        assert_eq!(allowed["results"][0]["outcome"], "applied");
        assert!(state.resident("queued").unwrap().queue.is_empty());
    });

    let forked = state
        .update(cx, |state, cx| {
            call(state, cx, "caller", change("source", RunOp::Fork))
        })
        .unwrap();
    assert_eq!(forked["results"][0]["outcome"], "applied");
    let fork = forked["results"][0]["new_thread_id"].as_str().unwrap();
    assert_ne!(fork, "source");
    state.read(|state| {
        assert_eq!(state.find_meta(fork).unwrap().title, "Thread source (fork)");
    });
}

/// The model references only threads inside its grant's scope; a reference across a project
/// carries the boundary notice and leaves the referenced thread as it was.
#[test]
fn reference_stays_within_the_grant_scope_and_marks_boundaries() {
    let store = TestStore::new("tcode-control-reference");
    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    state.update(cx, |state, _| {
        state
            .sessions
            .push(thread("caller", "a", ProviderKind::ClaudeCode));
        state
            .sessions
            .push(thread("near", "a", ProviderKind::ClaudeCode));
        state
            .sessions
            .push(thread("far", "b", ProviderKind::ClaudeCode));
    });
    let reference = |id: &str| {
        ControlOp::Reference(ReferenceParams {
            thread_id: id.into(),
            reason: None,
        })
    };
    state.update(cx, |state, cx| {
        assert_eq!(
            call(state, cx, "caller", reference("far")),
            Err("refused: thread far is outside your grant's scope (project)".into())
        );
        let near = call(state, cx, "caller", reference("near")).unwrap();
        assert_eq!(near["boundary_notice"], false);

        let mut caller = state.find_meta("caller").unwrap();
        caller.control_grant = Some(ControlGrant {
            scope: ControlScope::Host,
            tiers: [ControlTier::Observe, ControlTier::Operate]
                .into_iter()
                .collect(),
            ..ControlGrant::none()
        });
        state.store_meta(caller, cx);
        let far_before = state.find_meta("far").unwrap();
        let far = call(state, cx, "caller", reference("far")).unwrap();
        assert_eq!(far["boundary_notice"], true);
        assert_eq!(state.find_meta("far").unwrap(), far_before);

        let references = state.find_meta("caller").unwrap().references;
        assert_eq!(
            references
                .iter()
                .map(|reference| (
                    reference.thread_id.as_str(),
                    reference.by,
                    reference.boundary_notice
                ))
                .collect::<Vec<_>>(),
            [
                ("near", ReferenceOrigin::Model, false),
                ("far", ReferenceOrigin::Model, true)
            ]
        );
    });
}

/// `git` answers with what git did: checkout refuses a dirty tree and a thread whose turn is
/// running, commit reports git's output, and a clean tree then checks out.
#[test]
fn git_answers_with_the_outcome_and_refuses_unsafe_checkouts() {
    let store = TestStore::new("tcode-control-git");
    let root = store.root().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    for args in [
        &["init", "-b", "main"][..],
        &["config", "commit.gpgsign", "false"],
        &["config", "core.hooksPath", ".no-hooks"],
        &["config", "user.name", "tcode"],
        &["config", "user.email", "tcode@localhost"],
    ] {
        run_git(&root, args).unwrap();
    }
    std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
    run_git(&root, &["add", "tracked.txt"]).unwrap();
    run_git(&root, &["commit", "-m", "initial"]).unwrap();
    run_git(&root, &["branch", "topic"]).unwrap();
    std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();

    let cx = &mut TestAppContext::default();
    let state = host(&store, cx);
    state.update(cx, |state, _| {
        state
            .sessions
            .push(thread("caller", "p", ProviderKind::ClaudeCode));
        let mut target = thread("target", "p", ProviderKind::Codex);
        target.cwd = root.clone();
        park(state, ActiveSession::new(target, false, Vec::new()));
    });
    let git = |cx: &mut TestAppContext, op: GitOp| {
        let (reply, response) = smol::channel::bounded(1);
        state.update(cx, |state, cx| {
            state.handle_control_op(
                "caller".into(),
                ControlOp::Git(GitParams {
                    thread_id: "target".into(),
                    op,
                    request_id: None,
                }),
                reply,
                cx,
            )
        });
        cx.run_until(|_| !response.is_empty());
        response.try_recv().unwrap().unwrap()
    };
    let checkout = || GitOp::CheckoutBranch {
        branch: "topic".into(),
    };

    let dirty = git(cx, checkout());
    assert_eq!(dirty["outcome"], "failed");
    assert_eq!(dirty["reason"], "the working tree has uncommitted changes");

    let committed = git(
        cx,
        GitOp::Commit {
            message: "Record the change".into(),
            included: None,
        },
    );
    assert_eq!(committed["outcome"], "applied");
    assert!(
        committed["output"]
            .as_str()
            .unwrap()
            .contains("Record the change")
    );

    state.update(cx, |state, _| {
        state.resident_mut("target").unwrap().turn_in_flight = true;
    });
    let busy = git(cx, checkout());
    assert_eq!(busy["outcome"], "skipped");
    state.update(cx, |state, _| {
        state.resident_mut("target").unwrap().turn_in_flight = false;
    });

    assert_eq!(git(cx, checkout())["outcome"], "applied");
    assert_eq!(
        run_git(&root, &["branch", "--show-current"])
            .unwrap()
            .trim(),
        "topic"
    );
}
