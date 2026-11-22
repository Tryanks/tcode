use super::*;

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

    const RECORDING: &str = include_str!("../fixtures/monitor_wakeups_child.jsonl");

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
            assert!(text.contains("The user answers it."));
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
        // The lead's card answers what its child waits on while it is there.
        assert!(
            cx.seen()
                .status("parent")
                .child_requests
                .iter()
                .any(|waiting| waiting.child_id == "child"
                    && waiting
                        .approvals
                        .iter()
                        .any(|request| request.id == request_id)),
            "the lead's status carries its child's request"
        );
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
        assert!(cx.seen().status("parent").child_requests.is_empty());
        // The lead's log keeps how it was answered, for its card to show.
        assert_eq!(
            cx.seen()
                .timeline("parent")
                .child_request_answers
                .get(&("child".to_owned(), request_id.clone())),
            Some(&agent::ChildRequestAnswer::Approval {
                decision: decision.clone(),
                label: (!manual).then(|| offered.first().unwrap().label.clone()),
            })
        );
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
/// its policy; the owner's own turn is held to neither.
#[test]
fn control_calls_in_a_members_turn_stay_inside_the_space() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-space");
    let root = store.root().join("project-a");
    let mut project = tcode_core::project::Project::from_root(root.clone());
    project.id = "a".into();
    store.upsert_project(&project).unwrap();
    stored_thread(&store, "caller", ProviderKind::Codex, &root, |meta| {
        meta.project_id = Some("a".into());
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
/// the user does when they keep subagent answers to themselves. An answer is
/// for a pending request, every question and no other; a question is never
/// answered automatically.
#[test]
fn child_questions_reach_the_lead_and_respond_input_answers_them() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-child-questions");
    let fleet = Fleet::new(cx, &store, ProviderKind::Codex, &["child"], |_| {});
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
            optional: false,
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
            optional: false,
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
                "The user answers it."
            } else {
                "respond_input"
            }),
            "{text}"
        );
        for (caller, request, answers, error) in [
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
        assert!(
            cx.seen()
                .status("parent")
                .child_requests
                .iter()
                .any(|waiting| waiting.child_id == "child"
                    && waiting
                        .user_input
                        .as_ref()
                        .is_some_and(|pending| pending.request_id == request_id)),
            "the lead's status carries its child's question"
        );
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
            request_id: request_id.clone(),
            answers: sent.clone(),
        }]);
        cx.run_until_seen(|view| view.status("child").pending_user_input.is_none());
        assert!(cx.seen().status("parent").child_requests.is_empty());
        assert_eq!(
            cx.seen()
                .timeline("parent")
                .child_request_answers
                .get(&("child".to_owned(), request_id.clone())),
            Some(&agent::ChildRequestAnswer::Input { answers: sent })
        );
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
