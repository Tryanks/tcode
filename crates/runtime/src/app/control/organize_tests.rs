use super::*;
use crate::app::test_support::*;
use control_mcp::{GetThreadParams, ReviewItemInput};
use tcode_core::pull_request::{self, PullRequestKey, PullRequestSource};
use tcode_core::pull_request_watch::PullRequestWatch;
use tcode_protocol::{ClientPayload, HostMessage, Query, QueryResponse, TurnDue};

fn thread(id: &str, cwd: &Path) -> SessionMeta {
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, cwd.to_path_buf(), None);
    meta.id = id.into();
    meta.title = format!("Thread {id}");
    meta.project_id = Some("p".into());
    meta
}

/// Run one `tcode_control` call from `caller` the way the broker does, and wait for its answer.
fn call(
    state: &TestEntity,
    cx: &mut TestAppContext,
    caller: &str,
    op: ControlOp,
) -> Result<serde_json::Value, String> {
    let (reply, answer) = smol::channel::bounded(1);
    state.update(cx, |state, cx| {
        state.handle_control_op(caller.into(), op, reply, cx)
    });
    cx.run_until_recv(&answer)
}

fn update(changes: Vec<ThreadChange>, dry_run: bool) -> ControlOp {
    ControlOp::UpdateThreads(UpdateThreadsParams {
        changes,
        dry_run: Some(dry_run),
    })
}

fn change(thread_id: &str, op: ThreadOp, expect: Option<Expect>) -> ThreadChange {
    ThreadChange {
        thread_id: thread_id.into(),
        op,
        expect,
    }
}

fn results(answer: &serde_json::Value) -> Vec<BatchResult> {
    serde_json::from_value(answer["results"].clone()).unwrap()
}

/// A preview changes nothing and names what a settle would do; the real batch re-checks every
/// item, skips the running thread and the one with a message waiting as busy and the stale one
/// as changed, and reports the side effects the settle actually had. Unsettle restores none of
/// them.
#[test]
fn update_threads_previews_then_settles_with_its_real_side_effects() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-update-threads");
    let root = store.root().clone();
    for id in ["caller", "done", "running", "queued", "stale"] {
        stored_thread(
            &store,
            id,
            ProviderKind::ClaudeCode,
            &root.join(id),
            |meta| {
                meta.project_id = Some("p".into());
                if id == "done" {
                    meta.pinned_at = Some(1);
                    pull_request::link_pull_request(
                        &mut meta.pull_requests,
                        PullRequestKey::new("github.com", "sample/project", 7),
                        "https://github.com/sample/project/pull/7".into(),
                        PullRequestSource::Manual,
                        1,
                        true,
                    );
                    meta.pull_requests[0].watch = Some(PullRequestWatch::new(1));
                }
            },
        );
    }
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    // `done` ran a turn and its provider idles; `running` is in one; `queued` has a
    // message of the user's waiting for its time.
    let done = first_turn(&state, cx, &providers, "done", 0);
    done.play([AgentEvent::TurnCompleted {
        turn_id: "turn".into(),
        status: agent::TurnStatus::Completed,
        usage: None,
    }]);
    cx.run_until_seen(|view| view.status("done").running_turn.is_none());
    first_turn(&state, cx, &providers, "running", 1);
    state.open(cx, "queued");
    state
        .command(
            cx,
            Command::ScheduleTurn {
                session_id: "queued".into(),
                text: "Later".into(),
                attachment_paths: Vec::new(),
                due: TurnDue::AfterSecs(3600),
            },
        )
        .unwrap();
    // What a model reads before it writes: the thread as get_thread shows it.
    let shown = call(
        &state,
        cx,
        "caller",
        ControlOp::GetThread(GetThreadParams {
            thread_id: "done".into(),
        }),
    )
    .unwrap();
    let last_activity = shown["last_activity_at"].as_u64().unwrap();
    cx.clock.advance(Duration::from_secs(60));

    let batch = |dry_run| {
        update(
            vec![
                change(
                    "done",
                    ThreadOp::Settle,
                    Some(Expect {
                        last_activity_at: Some(last_activity),
                        state: Some("active".into()),
                        ..Default::default()
                    }),
                ),
                change("running", ThreadOp::Settle, None),
                change("queued", ThreadOp::Settle, None),
                change(
                    "stale",
                    ThreadOp::Settle,
                    Some(Expect {
                        max_position: Some(41),
                        ..Default::default()
                    }),
                ),
            ],
            dry_run,
        )
    };
    let skipped = |target: &str, reason: &str| BatchResult {
        target: target.into(),
        outcome: Outcome::Skipped,
        reason: Some(reason.into()),
        side_effects: None,
    };
    let preview = results(&call(&state, cx, "caller", batch(true)).unwrap());
    assert_eq!(preview[0].outcome, Outcome::WouldApply);
    let foretold = preview[0].side_effects.clone();
    assert!(
        foretold.as_ref().is_some_and(|effects| !effects.is_empty()),
        "a preview names what the settle would do"
    );
    assert_eq!(
        preview[1..],
        [
            skipped("running", "busy"),
            skipped("queued", "busy"),
            skipped("stale", "changed_since_preview"),
        ]
    );
    cx.run_until_parked();
    let shown = cx.seen().session("done").unwrap();
    assert!(!shown.is_settled() && shown.pinned_at.is_some());
    assert!(shown.pull_requests[0].watch.is_some());
    assert!(
        !audited_in(cx.seen(), "done", "settle"),
        "a preview audits nothing"
    );

    let applied = results(&call(&state, cx, "caller", batch(false)).unwrap());
    assert_eq!(
        applied,
        vec![
            BatchResult {
                target: "done".into(),
                outcome: Outcome::Applied,
                reason: None,
                side_effects: foretold,
            },
            skipped("running", "busy"),
            skipped("queued", "busy"),
            skipped("stale", "changed_since_preview"),
        ],
        "the settle did what its preview said"
    );
    assert!(
        matches!(done.next_command(cx), SessionCommand::Shutdown),
        "the settled thread's provider is let go"
    );
    cx.run_until_seen(|view| audited_in(view, "done", "settle"));
    let shown = cx.seen().session("done").unwrap();
    assert!(shown.is_settled() && shown.pinned_at.is_none());
    assert!(shown.pull_requests[0].watch.is_none());
    for id in ["running", "queued", "stale"] {
        assert!(!cx.seen().session(id).unwrap().is_settled(), "{id}");
    }
    assert_eq!(cx.seen().status("queued").queued_messages.len(), 1);

    let stale_state = call(
        &state,
        cx,
        "caller",
        update(
            vec![change(
                "done",
                ThreadOp::Unsettle,
                Some(Expect {
                    state: Some("active".into()),
                    ..Default::default()
                }),
            )],
            false,
        ),
    )
    .unwrap();
    assert_eq!(
        results(&stale_state),
        vec![skipped("done", "changed_since_preview")]
    );
    let unsettled = call(
        &state,
        cx,
        "caller",
        update(vec![change("done", ThreadOp::Unsettle, None)], false),
    )
    .unwrap();
    assert_eq!(results(&unsettled)[0].outcome, Outcome::Applied);
    cx.run_until_parked();
    let shown = cx.seen().session("done").unwrap();
    assert_eq!(shown.settled_override, Some(SettledOverride::Active));
    assert!(shown.pinned_at.is_none() && shown.pull_requests[0].watch.is_none());
}

/// Whether the client's copy of thread `id`'s log records `caller` doing `action` to it.
fn audited_in(view: &ClientView, id: &str, action: &str) -> bool {
    view.timeline(id).entries.iter().any(|entry| {
        matches!(
            &entry.content,
            EntryContent::ControlAction(made)
                if made.actor_thread_id == "caller" && made.action == action
        )
    })
}

/// Follow-up is stamped by the host's clock, not the client's, and a thread put on it again
/// keeps the time it was first added.
#[test]
fn follow_up_carries_the_hosts_time_and_keeps_it_when_set_again() {
    let store = TestStore::new("tcode-control-follow-up-since");
    store.upsert_meta(&thread("follow", store.root())).unwrap();
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.subscribe(cx, Topic::Index);
    let added = cx.now_secs();
    let follow_up = |note: Option<&str>| Command::SetFollowUp {
        session_id: "follow".into(),
        on: true,
        note: note.map(Into::into),
    };
    state.dispatch_command(cx, 1, follow_up(None));
    cx.run_until_parked();
    cx.clock.advance(Duration::from_secs(3600));
    state.dispatch_command(cx, 2, follow_up(Some("check the deploy")));
    cx.run_until_parked();
    assert_eq!(
        cx.seen().session("follow").unwrap().follow_up,
        Some(FollowUp {
            since: added,
            note: Some("check the deploy".into()),
        })
    );
}

/// Follow-up, parked and the Suggest tier hold back the automatic rules a plain thread falls
/// to; releasing each lets the rule apply.
#[test]
fn follow_up_parked_and_the_suggest_tier_hold_automatic_settlement() {
    let store = TestStore::new("tcode-control-protections");
    // The threads' activity was ten days before the host starts.
    let stamp = test_start_millis();
    for id in ["plain", "follow", "parked"] {
        let mut meta = thread(id, store.root());
        match id {
            "follow" => {
                meta.follow_up = Some(FollowUp {
                    since: 1,
                    note: None,
                })
            }
            "parked" => meta.parked_at = Some(1),
            _ => {}
        }
        store.upsert_meta(&meta).unwrap();
        store
            .append_event(
                id,
                stamp,
                &AgentEvent::TurnCompleted {
                    turn_id: "finished".into(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                },
            )
            .unwrap();
    }
    std::fs::write(
        store.root().join("settings.json"),
        r#"{"settlement_automation":"suggest"}"#,
    )
    .unwrap();
    let cx = &mut TestAppContext::default();
    cx.clock.advance(Duration::from_secs(10 * 86_400));
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.subscribe(cx, Topic::Index);
    state.update(cx, |state, cx| state.start_activity(cx));
    cx.run_until_parked();
    let settled = |cx: &TestAppContext| {
        ["plain", "follow", "parked"].map(|id| cx.seen().session(id).unwrap().is_settled())
    };
    assert_eq!(
        settled(cx),
        [false, false, false],
        "suggest settles nothing"
    );

    state.dispatch_command(
        cx,
        1,
        Command::PatchSettings {
            patch: SettingsPatch::SettlementAutomation(
                tcode_core::control::SettlementAutomation::ExplicitSignals,
            ),
        },
    );
    cx.run_until_parked();
    assert_eq!(settled(cx), [true, false, false]);

    state.dispatch_command(
        cx,
        2,
        Command::SetFollowUp {
            session_id: "follow".into(),
            on: false,
            note: None,
        },
    );
    state.dispatch_command(
        cx,
        3,
        Command::SetParked {
            session_id: "parked".into(),
            parked: false,
        },
    );
    cx.run_until_parked();
    assert_eq!(settled(cx), [true, true, true]);
}

/// A reviews.json, an activity log and a secrets.json this build cannot read do not stop the
/// host: a review is still recorded, the first record refused saving says why, and no file is
/// written over.
#[test]
fn unreadable_own_files_do_not_stop_the_host_and_are_never_written_over() {
    let store = TestStore::new("tcode-control-unreadable");
    std::fs::create_dir_all(store.root()).unwrap();
    let torn = "[{\"id\":";
    let files = ["reviews.json", "control_activity.json"];
    for name in files {
        std::fs::write(store.root().join(name), torn).unwrap();
    }
    for id in ["caller", "old"] {
        store.upsert_meta(&thread(id, store.root())).unwrap();
    }
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    for request in ["review-1", "review-2"] {
        let review = ControlOp::RecordReview(RecordReviewParams {
            items: vec![ReviewItemInput {
                thread_id: "old".into(),
                group: "can_settle".into(),
                confidence: "clear_evidence".into(),
                evidence: vec!["old#12: PR merged".into()],
                suggestion: None,
            }],
            coverage: "1 project, 2 threads".into(),
            request_id: Some(request.into()),
        });
        assert_eq!(
            call(&state, cx, "caller", review).unwrap()["status"],
            "recorded"
        );
    }
    let refusals = cx
        .drain_outgoing()
        .into_iter()
        .filter(|message| {
            matches!(
                message,
                HostMessage::Event(envelope) if matches!(
                    &envelope.event,
                    ServerEvent::Runtime(RuntimeEvent::Error(RuntimeError::FileUnreadable { .. }))
                )
            )
        })
        .count();
    assert_eq!(refusals, 2, "one for each file written");
    state.update(cx, |state, _| state.close_store().unwrap());
    for name in files {
        assert_eq!(
            std::fs::read_to_string(store.root().join(name)).unwrap(),
            torn
        );
    }
}

/// A review is stored and announced without touching any thread, and survives a restart.
#[test]
fn a_recorded_review_changes_no_thread_and_is_read_back_after_a_restart() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-review");
    for id in ["caller", "old"] {
        store.upsert_meta(&thread(id, store.root())).unwrap();
    }
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.subscribe(cx, Topic::Index);
    let before = cx.seen().session("old").unwrap().clone();
    let review = |group: &str| {
        ControlOp::RecordReview(RecordReviewParams {
            items: vec![ReviewItemInput {
                thread_id: "old".into(),
                group: group.into(),
                confidence: "clear_evidence".into(),
                evidence: vec!["old#12: PR merged".into()],
                suggestion: Some("settle".into()),
            }],
            coverage: "1 project, 2 threads".into(),
            request_id: Some("review-1".into()),
        })
    };
    assert!(call(&state, cx, "caller", review("done")).is_err());
    let recorded = call(&state, cx, "caller", review("can_settle")).unwrap();
    assert_eq!(recorded["status"], "recorded");
    assert_eq!(
        call(&state, cx, "caller", review("can_settle")).unwrap(),
        recorded,
        "a retry records nothing twice"
    );
    assert!(cx.drain_outgoing().iter().any(|message| matches!(
        message,
        HostMessage::Event(envelope) if matches!(
            &envelope.event,
            ServerEvent::Runtime(RuntimeEvent::Notice(RuntimeNotice::ReviewRecorded { items: 1, .. }))
        )
    )));
    cx.run_until_parked();
    assert_eq!(cx.seen().session("old"), Some(&before));

    // A restarted host is another process, which nothing the stopped host left running reaches.
    let cx = &mut TestAppContext::default();
    let restarted = cx.new_entity(TestClientState::new((*store).clone()));
    restarted.deliver(
        cx,
        5,
        ClientPayload::Query(Query::ReviewRecords { limit: 10 }),
    );
    let (records, _) = cx.run_until_sent(|message| match message {
        HostMessage::QueryResult {
            id: 5,
            result: Ok(QueryResponse::ReviewRecords(records)),
        } => Some(records.clone()),
        _ => None,
    });
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, "review-1");
    assert_eq!(records[0].thread_id, "caller");
    assert_eq!(records[0].items[0].group, "can_settle");
}
