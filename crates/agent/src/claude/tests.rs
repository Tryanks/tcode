use super::*;

/// A `CLAUDE_CONFIG_DIR` the user set in the profile's environment is
/// where the CLI keeps its transcripts, whatever `HOME` says.
#[test]
fn config_dir_follows_a_configured_claude_config_dir_over_home() {
    let launch_env = LaunchEnv {
        env: [("CLAUDE_CONFIG_DIR", "/configured"), ("HOME", "/home")]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        home: None,
    };
    assert_eq!(config_dir(&launch_env), Some(PathBuf::from("/configured")));
}

/// claude 2.1.318 reads its account file as `join(CLAUDE_CONFIG_DIR ??
/// home, ".claude.json")`: inside a configured directory, but beside
/// `~/.claude`, not in it, for the default account.
#[test]
fn account_file_sits_in_a_configured_dir_and_beside_dot_claude_otherwise() {
    assert_eq!(
        account_file_in(Some("/configured".into()), Some("/home".into())),
        Some(PathBuf::from("/configured/.claude.json"))
    );
    assert_eq!(
        account_file_in(None, Some("/home".into())),
        Some(PathBuf::from("/home/.claude.json"))
    );
}

#[test]
fn the_settings_claude_applied_are_read_back_in_tcodes_terms() {
    // `get_settings`' `applied` {model, effort, ultracode}: the control
    // schema of Claude Code 2.1.322; the values are made up.
    let answer = |sent: &str, applied: Value| {
        let mut mapper = Mapper::new();
        mapper.applied_settings = ClaudeLaunchOptions::resolve(
            Some("opus"),
            &[select(crate::EFFORT_OPTION_ID, json!(sent))],
        );
        let read = mapper.settings_read_request();
        assert_eq!(read["request"], json!({"subtype": "get_settings"}));
        mapper.on_message(json!({"type":"control_response", "response": {
            "subtype":"success", "request_id":read["request_id"],
            "response":{"applied":applied}
        }}))
    };
    let effort = |events: &[AgentEvent]| match events.last() {
        Some(AgentEvent::ProviderOptions { selections, .. }) => selections[0].value.clone(),
        _ => panic!("no effort in {events:?}"),
    };
    let events = answer(
        "ultracode",
        json!({"model":"claude-opus-5-5[1m]", "effort":"xhigh", "ultracode":true}),
    );
    assert!(matches!(events.first(), Some(AgentEvent::SessionModel { model }) if model == "opus"));
    assert_eq!(effort(&events), json!("ultracode"));
    assert_eq!(
        effort(&answer(
            "ultracode",
            json!({"effort":"xhigh", "ultracode":false})
        )),
        json!("xhigh")
    );
    assert_eq!(effort(&answer("low", json!({"effort":null}))), Value::Null);
}

/// `get_settings`' `effective` (the merged settings) and the
/// `apply_flag_settings` request are Claude Code 2.1.323's control schema;
/// the values are made up.
#[test]
fn bash_edit_diff_is_turned_on_unless_the_users_settings_decide_it() {
    let read_with = |effective: Value| {
        let mut mapper = Mapper::new();
        let read = mapper.settings_read_request();
        mapper.on_message(json!({"type":"control_response", "response": {
                "subtype":"success", "request_id":read["request_id"],
                "response":{"effective":effective, "sources":[], "applied":{"model":"claude-haiku-4-5","effort":null}}
            }}));
        mapper
    };
    let mut mapper = read_with(json!({"model":"haiku"}));
    let [apply] = mapper.take_outgoing().try_into().expect("one request");
    assert_eq!(
        apply["request"],
        json!({"subtype":"apply_flag_settings", "settings":{"bashEditDiffEnabled":true}})
    );
    let events = mapper.on_message(json!({"type":"control_response", "response": {
            "subtype":"error", "request_id":apply["request_id"], "error":"flag settings are read-only here"
        }}));
    assert!(
        matches!(events.as_slice(), [AgentEvent::Warning { message }]
                if message.contains("flag settings are read-only here")),
        "{events:?}"
    );

    let mut mapper = read_with(json!({"bashEditDiffEnabled":false}));
    assert!(mapper.take_outgoing().is_empty());
}

#[test]
fn native_permission_switches_publish_only_confirmed_values() {
    let mut mapper = Mapper::new();
    for mode in [
        "default",
        "acceptEdits",
        "auto",
        "dontAsk",
        "bypassPermissions",
    ] {
        let request = mapper.set_permission_mode_request_str(mode);
        assert_eq!(
            request["request"],
            json!({"subtype":"set_permission_mode", "mode":mode})
        );
        let events = mapper.on_message(json!({"type":"control_response", "response": {
            "subtype":"success", "request_id":request["request_id"], "response":{}
        }}));
        assert!(
            matches!(events.as_slice(), [AgentEvent::ProviderOptions { selections, .. }]
                if selections.iter().any(|selection| selection.id == "permissionMode" && selection.value == mode))
        );
    }
    let request = mapper.set_permission_mode_request_str("auto");
    let events = mapper.on_message(json!({"type":"control_response", "response": {
        "subtype":"error", "request_id":request["request_id"], "error":"unsupported mode"
    }}));
    assert!(
        matches!(events.as_slice(), [AgentEvent::Warning { message }] if message.contains("unsupported mode"))
    );

    let request = mapper.set_permission_mode_request_str("auto");
    let events = mapper.on_message(json!({"type":"control_response", "response": {
            "subtype":"success", "request_id":request["request_id"], "response":{"permissionMode":"dontAsk"}
        }}));
    assert!(
        matches!(events.as_slice(), [AgentEvent::ProviderOptions { selections, .. }] if selections[0].value == "dontAsk")
    );

    let request = mapper.set_permission_mode_request_str("auto");
    let reason = "Cannot set permission mode to auto: auto mode unavailable for this model";
    let events = mapper.on_message(json!({"type":"control_response", "response": {
        "subtype":"error", "request_id":request["request_id"],
        "error":reason, "error_code":"auto_mode_model"
    }}));
    assert!(
        events.iter().any(|event| matches!(event,
                AgentEvent::ProviderOptions { descriptors, selections }
                if selections[0].value == "dontAsk" && matches!(&descriptors[0],
                    OptionDescriptor::Select { options, .. }
                    if options.iter().any(|option| option.value == "auto"
                        && option.unavailable.as_deref() == Some(reason))))),
        "the provider's explicit auto unavailability must reach the picker: {events:?}"
    );

    let mut auto = Mapper::new_configured("auto".into());
    let events = auto.on_message(json!({"type":"system", "subtype":"init", "session_id":"native", "permissionMode":"default"}));
    let descriptor = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ProviderOptions { descriptors, .. } => descriptors.first(),
            _ => None,
        })
        .unwrap();
    assert!(
        matches!(descriptor, OptionDescriptor::Select { options, .. }
            if options.iter().any(|option| option.value == "auto" && option.unavailable.is_some()))
    );
    let events = auto.on_message(
        json!({"type":"system", "subtype":"init", "session_id":"native", "permissionMode":"plan"}),
    );
    assert!(
        matches!(events.as_slice(), [AgentEvent::ProviderOptions { descriptors, selections }]
            if selections[0].value == "plan" && matches!(&descriptors[0], OptionDescriptor::Select { options, .. }
                if options.iter().any(|option| option.value == "plan" && option.unavailable.is_some())))
    );

    let mut supported = Mapper::new_configured("auto".into());
    supported.on_message(json!({"type":"system", "subtype":"init", "session_id":"supported", "permissionMode":"auto"}));
    let events = supported.on_message(json!({"type":"system", "subtype":"init", "session_id":"supported", "permissionMode":"plan"}));
    assert!(
        matches!(events.as_slice(), [AgentEvent::ProviderOptions { descriptors, selections }]
            if selections[0].value == "plan" && matches!(&descriptors[0], OptionDescriptor::Select { options, .. }
                if options.iter().any(|option| option.value == "plan" && option.unavailable.is_some())
                && options.iter().any(|option| option.value == "auto" && option.unavailable.is_none()))),
        "native plan entry must preserve confirmed auto availability: {events:?}"
    );

    for confirmation in ["control_response", "init"] {
        let mut recovered = Mapper::new_configured("auto".into());
        recovered.on_message(json!({"type":"system", "subtype":"init", "session_id":"recovered", "permissionMode":"default"}));
        let message = if confirmation == "init" {
            json!({"type":"system", "subtype":"init", "session_id":"recovered", "permissionMode":"auto"})
        } else {
            let request = recovered.set_permission_mode_request_str("auto");
            json!({"type":"control_response", "response": {
                "subtype":"success", "request_id":request["request_id"], "response":{"permissionMode":"auto"}
            }})
        };
        let events = recovered.on_message(message);
        assert!(
            matches!(events.as_slice(), [AgentEvent::ProviderOptions { descriptors, selections }]
                if selections[0].value == "auto" && matches!(&descriptors[0], OptionDescriptor::Select { options, .. }
                    if options.iter().any(|option| option.value == "auto" && option.unavailable.is_none()))),
            "{confirmation} confirmation must restore auto availability: {events:?}"
        );
    }
}

#[test]
fn native_permission_requests_apply_only_the_chosen_suggestion_group() {
    let input = json!({"file_path":"file.rs", "content":"changed"});
    let session =
        json!({"type":"addRules", "rules":[{"toolName":"Write"}], "destination":"session"});
    let user = json!({"type":"setMode", "mode":"acceptEdits", "destination":"userSettings"});
    for name in [
        "Write",
        "EnterPlanMode",
        "mcp__tcode_control__report_result",
    ] {
        for choice in [
            "allow",
            "deny",
            "allow_suggestion:0",
            "allow_suggestion:1",
            "cancel",
        ] {
            let mut mapper = Mapper::new();
            let events = mapper.on_message(
                json!({"type":"control_request", "request_id":"native", "request": {
                    "subtype":"can_use_tool", "tool_name":name, "input":input,
                    "permission_suggestions":[session, user]
                }}),
            );
            let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
                panic!("{events:?}")
            };
            assert_eq!(
                request
                    .options
                    .iter()
                    .map(|option| option.id.as_str())
                    .collect::<Vec<_>>(),
                ["allow", "deny", "allow_suggestion:0", "allow_suggestion:1"]
            );
            assert_eq!(
                request.options[2].label,
                "Allow and remember for this session"
            );
            assert!(mapper.take_outgoing().is_empty());
            let decision = if choice == "cancel" {
                ApprovalDecision::Cancel
            } else {
                ApprovalDecision::Option(choice.into())
            };
            let response = mapper.build_approval_response("native", decision).unwrap();
            let body = &response["response"]["response"];
            if matches!(choice, "deny" | "cancel") {
                assert_eq!(body["behavior"], "deny");
                assert_eq!(
                    body["message"],
                    if choice == "cancel" {
                        "User cancelled tool execution."
                    } else {
                        "User declined tool execution."
                    }
                );
            } else {
                assert_eq!(body["behavior"], "allow");
                assert_eq!(body["updatedInput"], input);
            }
            match choice {
                "allow_suggestion:0" => {
                    assert_eq!(body["updatedPermissions"], json!([session]))
                }
                "allow_suggestion:1" => assert_eq!(body["updatedPermissions"], json!([user])),
                _ => assert!(body.get("updatedPermissions").is_none()),
            }
        }
    }
}

/// Every worktree goes through the host: the hooks are registered before
/// any message, and the host's answer to Claude's `WorktreeCreate`
/// callback reaches Claude as the hook's output.
#[test]
fn the_host_answers_claudes_worktree_hooks() {
    let mut mapper = Mapper::new();
    assert_eq!(
        mapper.initialize_request(),
        json!({"type":"control_request","request_id":"tcode-ctrl-1","request":{
            "subtype":"initialize",
            "hooks":{
                "WorktreeCreate":[{"hookCallbackIds":["tcode-worktree-create"],"onFailure":"block"}],
                "WorktreeRemove":[{"hookCallbackIds":["tcode-worktree-remove"],"onFailure":"block"}]
            }
        }})
    );
    feed(
        &mut mapper,
        r#"{"type":"control_request","request_id":"hook-1","request":{"subtype":"hook_callback","callback_id":"tcode-worktree-create","input":{"session_id":"native","transcript_path":"/home/me/.claude/projects/-work-repo/native.jsonl","cwd":"/work/repo","prompt_id":"prompt-1","hook_event_name":"WorktreeCreate","name":"feat"},"tool_use_id":"00000000-0000-4000-8000-000000000062"}}"#,
    );
    let answer = WorktreeAnswer::Ready {
        path: PathBuf::from("/data/worktrees/repo-1a2b3c4d/feat"),
        reused: false,
    };
    assert_eq!(
        mapper.worktree_response("hook-1", &answer),
        Some((
            json!({"type":"control_response","response":{
                "subtype":"success","request_id":"hook-1",
                "response":{"hookSpecificOutput":{
                    "hookEventName":"WorktreeCreate",
                    "worktreePath":"/data/worktrees/repo-1a2b3c4d/feat"
                }}
            }}),
            WorktreeRequest::Create {
                cwd: PathBuf::from("/work/repo"),
                name: "feat".into(),
            }
        ))
    );
}

/// An `isolation: "worktree"` subagent asks for `agent-<agent id>` before
/// its `task_started`; the worktree it is given is where that subagent
/// works, while the session stays where it is. Removal and a refusal answer
/// in the shapes Claude Code 2.1.319 accepts.
#[test]
fn a_subagent_works_in_the_worktree_its_hook_was_given() {
    let mut mapper = Mapper::new();
    mapper.cwd = Some(PathBuf::from("/work/repo"));
    feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg-1","role":"assistant","content":[{"type":"tool_use","id":"toolu_agent","name":"Agent","input":{"description":"Print working directory","prompt":"Run pwd","subagent_type":"general-purpose","isolation":"worktree"}}]},"parent_tool_use_id":null,"session_id":"native"}"#,
    );
    let events = feed(
        &mut mapper,
        r#"{"type":"control_request","request_id":"hook-1","request":{"subtype":"hook_callback","callback_id":"tcode-worktree-create","input":{"session_id":"native","transcript_path":"/home/me/.claude/projects/-work-repo/native.jsonl","cwd":"/work/repo","prompt_id":"prompt-1","hook_event_name":"WorktreeCreate","name":"agent-a0000000000000010"},"tool_use_id":"00000000-0000-4000-8000-000000000063"}}"#,
    );
    assert_eq!(
        events,
        [AgentEvent::WorktreeRequested {
            request_id: "hook-1".into(),
            request: WorktreeRequest::Create {
                cwd: PathBuf::from("/work/repo"),
                name: "agent-a0000000000000010".into(),
            },
        }]
    );
    let worktree = PathBuf::from("/data/worktrees/repo-1a2b3c4d/agent-a0000000000000010");
    mapper
        .worktree_response(
            "hook-1",
            &WorktreeAnswer::Ready {
                path: worktree.clone(),
                reused: false,
            },
        )
        .unwrap();
    let events = feed(
        &mut mapper,
        r#"{"type":"system","subtype":"task_started","task_id":"a0000000000000010","tool_use_id":"toolu_agent","description":"Print working directory","subagent_type":"general-purpose","is_backgrounded":true,"spawn_depth":1,"task_type":"local_agent","prompt":"Run pwd","session_id":"native"}"#,
    );
    assert!(
        events.contains(&AgentEvent::SessionMoved {
            from: PathBuf::from("/work/repo"),
            to: worktree.clone(),
            parent_item_id: Some("toolu_agent".into()),
        }),
        "{events:?}"
    );

    feed(
        &mut mapper,
        r#"{"type":"control_request","request_id":"hook-2","request":{"subtype":"hook_callback","callback_id":"tcode-worktree-remove","input":{"session_id":"native","cwd":"/work/repo","hook_event_name":"WorktreeRemove","worktree_path":"/data/worktrees/repo-1a2b3c4d/agent-a0000000000000010"}}}"#,
    );
    let (removed, _) = mapper
        .worktree_response("hook-2", &WorktreeAnswer::Removed)
        .unwrap();
    assert_eq!(
        removed,
        json!({"type":"control_response","response":{"subtype":"success","request_id":"hook-2","response":{}}})
    );
    feed(
        &mut mapper,
        r#"{"type":"control_request","request_id":"hook-3","request":{"subtype":"hook_callback","callback_id":"tcode-worktree-create","input":{"session_id":"native","cwd":"/work/repo","hook_event_name":"WorktreeCreate","name":"feat"}}}"#,
    );
    let (refused, request) = mapper
        .worktree_response(
            "hook-3",
            &WorktreeAnswer::Refused {
                error: "/work/repo is not a Git repository root".into(),
            },
        )
        .unwrap();
    assert_eq!(
        refused,
        json!({"type":"control_response","response":{"subtype":"error","request_id":"hook-3","error":"/work/repo is not a Git repository root"}})
    );
    assert!(matches!(request, WorktreeRequest::Create { name, .. } if name == "feat"));
    assert!(
        mapper
            .worktree_response("hook-3", &WorktreeAnswer::Removed)
            .is_none()
    );
}

/// The session's directory follows Claude in and out of worktrees: into
/// the worktree `EnterWorktree` made, back to where it worked before when
/// `ExitWorktree` leaves it (in the spelling Tcode started it with), and,
/// on resume, to a worktree of the thread's that `system/init` says Claude
/// restored. A directory the shell merely cd'd to, which `system/init`
/// reports too, is not a move. A subagent's own `EnterWorktree` moves only
/// that subagent.
#[cfg(unix)]
#[test]
fn the_session_directory_follows_claude_in_and_out_of_worktrees() {
    let directory = std::env::temp_dir().join(format!("agent-claude-cwd-{}", uuid::Uuid::new_v4()));
    let checkout = directory.join("repo");
    std::fs::create_dir_all(&checkout).unwrap();
    let link = directory.join("link");
    std::os::unix::fs::symlink(&checkout, &link).unwrap();
    let resolved = checkout.canonicalize().unwrap();
    let worktree = PathBuf::from("/data/worktrees/repo-1a2b3c4d/feat");
    let started = |cwd: &Path| {
        let mut mapper = Mapper::new();
        mapper.launch_cwd = Some(link.clone());
        mapper.cwd = Some(link.clone());
        mapper.worktrees = vec![worktree.clone()];
        let events = mapper.on_message(json!({"type":"system","subtype":"init","cwd":cwd,"session_id":"native","permissionMode":"default"}));
        (mapper, events)
    };
    let moved = |events: &[AgentEvent]| -> Vec<(PathBuf, PathBuf)> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::SessionMoved {
                    from,
                    to,
                    parent_item_id: None,
                } => Some((from.clone(), to.clone())),
                _ => None,
            })
            .collect()
    };

    let (mut mapper, events) = started(&resolved);
    assert_eq!(moved(&events), []);
    let shell = mapper.on_message(json!({"type":"system","subtype":"init","cwd":resolved.join("src"),"session_id":"native","permissionMode":"default"}));
    assert_eq!(moved(&shell), []);
    let tool = |id: &str, name: &str, result: Value| {
        [
            json!({"type":"assistant","message":{"id":format!("msg-{id}"),"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":{}}]},"parent_tool_use_id":null,"session_id":"native"}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"done"}]},"parent_tool_use_id":null,"session_id":"native","tool_use_result":result}),
        ]
    };
    let mut events = Vec::new();
    for frame in tool(
        "toolu_enter",
        "EnterWorktree",
        json!({"worktreePath":worktree,"message":"Created worktree. The session is now working in the worktree."}),
    ) {
        events.extend(mapper.on_message(frame));
    }
    assert_eq!(moved(&events), [(link.clone(), worktree.clone())]);
    // A subagent's call reaches the session as that subagent's.
    let events = mapper.on_message(json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_child_enter","content":"done"}]},"parent_tool_use_id":"toolu_agent","session_id":"native","tool_use_result":{"worktreePath":"/data/worktrees/repo-1a2b3c4d/child"}}));
    assert_eq!(moved(&events), []);
    let mut events = Vec::new();
    for frame in tool(
        "toolu_exit",
        "ExitWorktree",
        json!({"action":"keep","originalCwd":resolved,"worktreePath":worktree,"restoredCwd":resolved,"originalCwdMissing":false}),
    ) {
        events.extend(mapper.on_message(frame));
    }
    assert_eq!(moved(&events), [(worktree.clone(), link.clone())]);

    let (_, events) = started(&worktree);
    assert_eq!(moved(&events), [(link.clone(), worktree.clone())]);
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn fork_flag_requires_both_request_and_resume_cursor() {
    let resume = Some(ResumeCursor(json!({"session_id": "session-source"})));
    assert_eq!(
        resume_args(&resume, true, None),
        ["--resume", "session-source", "--fork-session"]
    );
    assert_eq!(
        resume_args(&resume, false, None),
        ["--resume", "session-source"]
    );
    assert!(resume_args(&None, true, None).is_empty());
}

/// A fork from a message ends at the entry the message answered, which
/// only the transcript names (entries as 2.1.318 writes them), and leaves
/// out `--resume-drops-turn`, which refuses a range of several turns.
#[test]
fn a_fork_from_a_message_ends_where_its_transcript_says_it_was_sent() {
    let dir = std::env::temp_dir().join(format!("agent-claude-fork-{}", uuid::Uuid::new_v4()));
    let project = dir.join("projects/-work");
    std::fs::create_dir_all(&project).unwrap();
    let entry = |kind: &str, uuid: &str, parent: Option<&str>| {
        json!({"type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": "source",
                "message": {"role": kind, "content": [{"type": "text", "text": "text"}]}})
        .to_string()
    };
    std::fs::write(
        project.join("source.jsonl"),
        [
            entry("user", "prompt-1", None),
            entry("assistant", "reply-1", Some("prompt-1")),
            entry("user", "prompt-2", Some("reply-1")),
            entry("assistant", "reply-2", Some("prompt-2")),
            entry("user", "prompt-3", Some("reply-2")),
        ]
        .join("\n"),
    )
    .unwrap();
    let resume = Some(ResumeCursor(json!({"session_id": "source"})));

    let anchor = fork_anchor(&resume, Some(&dir), "prompt-2").unwrap();
    assert_eq!(
        resume_args(&resume, true, Some(&anchor)),
        [
            "--resume",
            "source",
            "--fork-session",
            "--resume-session-at",
            "reply-1"
        ]
    );
    for refused in ["prompt-1", "not-in-the-session"] {
        assert!(
            matches!(
                fork_anchor(&resume, Some(&dir), refused),
                Err(AgentError::Provider(_))
            ),
            "{refused}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A launch asks for the native permission prompt in the selected mode
/// with a live switch to bypass allowed, puts Tcode's instructions in the
/// system prompt from a file, and leaves the user's launch arguments last
/// so they win.
#[test]
fn a_launch_passes_the_selected_mode_and_leaves_the_users_arguments_last() {
    let base = [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--verbose",
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
    ];
    let os = |args: &[&str]| -> Vec<std::ffi::OsString> { args.iter().map(Into::into).collect() };
    let launch = ClaudeLaunchOptions {
        model_id: Some("claude-opus-4-8".into()),
        effort: Some("high".into()),
        settings_json: Some(r#"{"fastMode":true}"#.into()),
        ..ClaudeLaunchOptions::default()
    };
    let mcp = [crate::McpRegistration {
        name: "tcode_pull_requests".into(),
        url: "http://127.0.0.1:1/mcp".into(),
        bearer_token: "token".into(),
    }];
    let mcp_config = mcp_args(&mcp);
    let mut expected = os(&base);
    expected.extend(os(&[
        "auto",
        "--allow-dangerously-skip-permissions",
        "--replay-user-messages",
        "--model",
        "claude-opus-4-8",
        "--effort",
        "high",
        "--settings",
        r#"{"fastMode":true}"#,
        "--resume",
        "native",
        &mcp_config[0],
        &mcp_config[1],
        "--append-system-prompt-file",
        "/tmp/instructions.md",
        "--permission-mode",
        "plan",
    ]));
    assert_eq!(
        launch_args(
            "auto",
            &launch,
            vec!["--resume".into(), "native".into()],
            &mcp,
            Some(Path::new("/tmp/instructions.md")),
            &["--permission-mode".into(), "plan".into()],
        ),
        expected
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
        if count == 0 {
            assert!(args.is_empty());
            continue;
        }
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--mcp-config");
        let config: Value = serde_json::from_str(&args[1]).unwrap();
        let servers = config["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), count);
        for (name, url, authorization) in [
            ("tcode_preview", "http://p", "Bearer p"),
            ("tcode_control", "http://o", "Bearer o"),
            ("tcode_computer_use", "http://c", "Bearer c"),
        ]
        .into_iter()
        .take(count)
        {
            assert_eq!(
                servers[name],
                json!({
                    "type": "http",
                    "url": url,
                    "headers": {"Authorization": authorization},
                })
            );
        }
    }
}

fn feed(mapper: &mut Mapper, line: &str) -> Vec<AgentEvent> {
    let msg: Value = serde_json::from_str(line).expect("valid json fixture line");
    mapper.on_message(msg)
}

/// [`feed`] without the list a task change also publishes, for tests of
/// the liveness count.
fn feed_count(mapper: &mut Mapper, line: &str) -> Vec<AgentEvent> {
    let mut events = feed(mapper, line);
    events.retain(|event| !matches!(event, AgentEvent::BackgroundTaskList { .. }));
    events
}

/// Every `result` publishes the current background-task count immediately
/// before its `TurnCompleted` (the runtime's only reliable zero signal).
/// Assert that contract and return the completion event.
#[track_caller]
fn turn_completed_after_count(events: &[AgentEvent], count: usize) -> &AgentEvent {
    let at = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
        .unwrap_or_else(|| panic!("no TurnCompleted in {events:?}"));
    assert!(
        at > 0
            && matches!(
                &events[at - 1],
                AgentEvent::BackgroundTasksChanged { count: published } if *published == count
            ),
        "TurnCompleted must follow BackgroundTasksChanged {{ count: {count} }}, got {events:?}"
    );
    &events[at]
}

#[test]
fn mid_stream_fallback_is_published_before_the_response_finishes() {
    // Claude Code 2.1.299 against a local SSE endpoint using the documented
    // fallback boundary; only stream, assistant and result messages retained.
    let fixture = include_str!("../../tests/fixtures/claude/mid-stream-fallback.jsonl");
    {
        let mut mapper = Mapper::new();
        mapper.start_turn();
        let mut switched = false;
        let mut completed = false;
        for line in fixture.lines() {
            let message: Value = serde_json::from_str(line).unwrap();
            let boundary = message
                .pointer("/message/content/0/type")
                .and_then(Value::as_str)
                == Some("fallback");
            let events = mapper.on_message(message);
            if boundary {
                assert!(!completed);
                let notices = events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::ServedModel { .. }))
                    .cloned()
                    .collect::<Vec<_>>();
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
                );
                assert_eq!(
                    notices,
                    [AgentEvent::ServedModel {
                        model: "claude-opus-4-8".into(),
                        from: Some("claude-fable-5".into()),
                        reason: None
                    }]
                );
                switched = true;
            } else if switched {
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, AgentEvent::ServedModel { .. })),
                    "later blocks must not repeat or undo the switch: {events:?}"
                );
            }
            if events
                .iter()
                .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
            {
                assert!(switched, "the switch must be visible before completion");
                completed = true;
            }
        }
        assert!(switched && completed);
    }
}

#[test]
fn model_refusal_fallback_publishes_served_model_before_completion() {
    let mut mapper = Mapper::new();
    mapper.start_turn();
    let events = feed(
        &mut mapper,
        r#"{"type":"system","subtype":"model_refusal_fallback","trigger":"refusal","direction":"retry","scope":"session","original_model":"claude-fable-5","fallback_model":"claude-opus-4-8","request_id":"req_123","api_refusal_category":"cyber","api_refusal_explanation":null,"refused_user_message_uuid":"00000000-0000-4000-8000-000000000064","content":"Safeguards flagged this message. Switched models.","session_id":"session-1","uuid":"system-1"}"#,
    );

    assert_eq!(
        events,
        [AgentEvent::ServedModel {
            model: "claude-opus-4-8".into(),
            from: Some("claude-fable-5".into()),
            reason: None,
        }]
    );
}

#[test]
fn classifier_refusal_result_fails_the_turn_with_claudes_text() {
    let mut mapper = Mapper::new();
    mapper.start_turn();

    let assistant_events = feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg-refusal","model":"<synthetic>","stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"This request triggered cyber content restrictions."},"content":[{"type":"text","text":"API Error: safeguards flagged this message. Details: `[cyber]`"}]},"parent_tool_use_id":null}"#,
    );
    assert!(
        assistant_events
            .iter()
            .any(|event| matches!(event, AgentEvent::Warning { .. }))
    );

    let events = feed(
        &mut mapper,
        r#"{"type":"result","subtype":"success","is_error":true,"stop_reason":"refusal","result":"API Error: safeguards flagged this message. Details: `[cyber]`","terminal_reason":"api_error","modelUsage":{"claude-fable-5":{"inputTokens":1,"outputTokens":0}}}"#,
    );

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Error { fatal: false, message } if message.contains("[cyber]")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnCompleted {
            status: TurnStatus::Failed,
            ..
        }
    )));
}

#[test]
fn replayed_user_message_exposes_native_turn_checkpoint() {
    let mut mapper = Mapper::new();
    let turn_id = mapper.start_turn();
    let events = feed(
        &mut mapper,
        r#"{"type":"user","uuid":"checkpoint-1","message":{"content":[{"type":"text","text":"hello"}]}}"#,
    );
    assert!(matches!(
        events.as_slice(),
        [AgentEvent::TurnCheckpoint {
            turn_id: actual_turn,
            checkpoint_id,
        }] if actual_turn == &turn_id && checkpoint_id == "checkpoint-1"
    ));

    // Tool-result user envelopes and later steering echoes cannot replace
    // the checkpoint associated with the opening prompt.
    assert!(feed(
            &mut mapper,
            r#"{"type":"user","uuid":"tool-result","message":{"content":[{"type":"tool_result","tool_use_id":"missing","content":"ok"}]}}"#,
        )
        .is_empty());
}

fn rewind_response(mapper: &mut Mapper, request: &Value, response: Value) -> Vec<AgentEvent> {
    feed(
        mapper,
        &json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request["request_id"],
                "response": response,
            }
        })
        .to_string(),
    )
}

/// Restoring files and conversation during a running turn: the turn is
/// stopped first, files are restored, and when the files fail the
/// conversation is still rewound, as Claude Code's own menu does.
#[test]
fn a_running_turn_is_stopped_before_files_and_a_files_failure_still_rewinds_the_conversation() {
    let mut mapper = Mapper::new();
    mapper.start_turn();
    let interrupt = mapper.begin_rewind("checkpoint-2".into(), RewindMode::FilesAndConversation);
    assert_eq!(interrupt["request"], json!({ "subtype": "interrupt" }));
    assert!(rewind_response(&mut mapper, &interrupt, json!({})).is_empty());
    let files = mapper.take_outgoing().remove(0);
    assert_eq!(
        files["request"],
        json!({ "subtype": "rewind_files", "user_message_id": "checkpoint-2" })
    );
    let error = "Failed to rewind: No file checkpoint found for this message.";
    let events = feed(
        &mut mapper,
        &json!({
            "type": "control_response",
            "response": {
                "subtype": "error",
                "request_id": files["request_id"],
                "error": error,
            }
        })
        .to_string(),
    );
    assert!(events.is_empty());
    let conversation = mapper.take_outgoing().remove(0);
    assert_eq!(
        conversation["request"],
        json!({
            "subtype": "rewind_conversation",
            "target_message_uuid": "checkpoint-2",
            "interrupt_if_running": true,
            "allow_summarized_target": true,
        })
    );
    let events = rewind_response(
        &mut mapper,
        &conversation,
        json!({ "rewound": true, "prefillText": "original prompt" }),
    );
    assert_eq!(
        serde_json::to_value(&events).unwrap(),
        json!([{
            "type": "rewind_completed",
            "checkpoint_id": "checkpoint-2",
            "mode": "conversation",
            "prefill": "original prompt",
            "later_history": "truncated",
            "incomplete": {
                "requested": "files_and_conversation",
                "failure": { "reason": "nothing_to_restore", "detail": error },
            },
        }])
    );
}

/// A preview is Claude's dry run of the files a rewind would restore; a
/// rewind of the conversation alone restores none and asks nothing.
#[test]
fn a_rewind_preview_is_a_dry_run_of_the_files() {
    let mut mapper = Mapper::new();
    assert_eq!(
        mapper.begin_preview("checkpoint-2".into(), RewindMode::FilesAndConversation),
        Some(
            json!({"type":"control_request","request_id":"tcode-ctrl-1","request":{
                "subtype":"rewind_files","user_message_id":"checkpoint-2","dry_run":true
            }})
        )
    );
    assert_eq!(
        mapper.begin_preview("checkpoint-2".into(), RewindMode::Conversation),
        None
    );
}

/// `rewind_conversation` stops a running turn itself, so the turn it
/// stops ends interrupted rather than failed.
#[test]
fn a_conversation_rewind_during_a_running_turn_ends_it_interrupted() {
    let mut mapper = Mapper::new();
    mapper.start_turn();
    mapper.begin_rewind("checkpoint-2".into(), RewindMode::Conversation);
    let events = feed(
        &mut mapper,
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"stopped"}"#,
    );
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert!(matches!(
        turn_completed_after_count(&events, 0),
        AgentEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            ..
        }
    ));
}

fn select(id: &str, value: Value) -> OptionSelection {
    OptionSelection {
        id: id.into(),
        value,
    }
}

fn settings(launch: &ClaudeLaunchOptions) -> Value {
    launch
        .settings_json
        .as_deref()
        .map(|settings| serde_json::from_str(settings).unwrap())
        .unwrap_or(Value::Null)
}

#[test]
fn effort_map_drives_the_effort_flag() {
    let catalog = crate::claude_manifest::test_catalog();
    let resolve = |model: &str, effort: &str| {
        ClaudeLaunchOptions::resolve_with(
            &catalog,
            Some(model),
            &[select("reasoningEffort", json!(effort))],
        )
    };
    // Mapped to another value: ultracode → xhigh, plus the ultracode setting.
    let launch = resolve("test-wide", "ultracode");
    assert_eq!(launch.effort.as_deref(), Some("xhigh"));
    assert_eq!(settings(&launch)["ultracode"], true);
    // Per-profile downgrade (max → high) and passthrough.
    assert_eq!(
        resolve("test-narrow", "max").effort.as_deref(),
        Some("high")
    );
    assert_eq!(resolve("test-wide", "max").effort.as_deref(), Some("max"));
    assert_eq!(
        resolve("test-fixed", "xhigh").effort.as_deref(),
        Some("max")
    );
    assert_eq!(
        resolve("test-wide", "bogus").effort.as_deref(),
        Some("medium")
    );
    assert_eq!(
        ClaudeLaunchOptions::resolve_with(&catalog, Some("test-wide"), &[])
            .effort
            .as_deref(),
        Some("medium")
    );
    assert_eq!(resolve("test-plain", "low").effort, None);
    // Unknown model: no descriptor to resolve against, so no flag.
    assert_eq!(resolve("test-unknown", "high").effort, None);
}

#[test]
fn parse_semver_from_version_output() {
    assert_eq!(
        crate::parse_semver("2.1.206 (Claude Code)"),
        Some((2, 1, 206))
    );
    assert_eq!(crate::parse_semver("2.1.169"), Some((2, 1, 169)));
    assert_eq!(crate::parse_semver("nonsense"), None);
}

#[test]
fn context_window_launch_semantics() {
    let catalog = crate::claude_manifest::test_catalog();
    let resolve = |model: &str, value: Option<Value>| {
        let selections: Vec<_> = value
            .map(|value| select("contextWindow", value))
            .into_iter()
            .collect();
        ClaudeLaunchOptions::resolve_with(&catalog, Some(model), &selections)
    };
    let auto_compact = |launch: &ClaudeLaunchOptions| settings(launch)["autoCompactWindow"].clone();

    // Default window of a 1M-default profile: the bare slug already runs
    // at 1M, so no suffix is sent (and none is needed for `1m` either).
    for value in [None, Some(json!("1m"))] {
        let launch = resolve("test-wide", value);
        assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
        assert!(launch.settings_json.is_none());
    }
    assert_eq!(catalog.resolved_context_window("test-wide", &[]), 1_000_000);
    // A suffixed id still resolves to the same model.
    assert_eq!(
        catalog.resolved_context_window("test-wide[1m]", &[]),
        1_000_000
    );

    // Below the largest window: bare slug plus early compaction.
    let launch = resolve("test-wide", Some(json!("200k")));
    assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
    assert_eq!(auto_compact(&launch), json!(200_000));
    let launch = resolve("test-wide", Some(json!(500_000)));
    assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
    assert_eq!(auto_compact(&launch), json!(500_000));

    // 200k-default profile: only the 1M expansion carries the suffix.
    for value in [Some(json!("200k")), None] {
        let launch = resolve("test-narrow", value);
        assert_eq!(launch.model_id.as_deref(), Some("test-narrow"));
        assert_eq!(auto_compact(&launch), json!(200_000));
    }
    let launch = resolve("test-narrow", Some(json!("1m")));
    assert_eq!(launch.model_id.as_deref(), Some("test-narrow[1m]"));
    assert!(launch.settings_json.is_none());
    let launch = resolve("test-narrow", Some(json!(500_000)));
    assert_eq!(launch.model_id.as_deref(), Some("test-narrow[1m]"));
    assert_eq!(auto_compact(&launch), json!(500_000));

    // Fixed window: no suffix, smaller values compact early.
    assert_eq!(
        catalog.resolved_context_window("test-fixed", &[]),
        1_000_000
    );
    let launch = resolve("test-fixed", Some(json!("1m")));
    assert_eq!(launch.model_id.as_deref(), Some("test-fixed"));
    assert!(launch.settings_json.is_none());
    let launch = resolve("test-fixed", Some(json!(500_000)));
    assert_eq!(launch.model_id.as_deref(), Some("test-fixed"));
    assert_eq!(auto_compact(&launch), json!(500_000));

    // An alias launches as the canonical slug with the same semantics.
    let launch = resolve("WIDE", Some(json!("200k")));
    assert_eq!(launch.model_id.as_deref(), Some("test-wide"));
    assert_eq!(auto_compact(&launch), json!(200_000));
    let launch = resolve("test-wide.0[1m]", None);
    assert_eq!(launch.model_id.as_deref(), Some("test-wide"));

    // No context data at all falls back to 200k and never emits settings.
    assert_eq!(catalog.resolved_context_window("test-plain", &[]), 200_000);
    assert_eq!(
        catalog.resolved_context_window("test-unknown", &[]),
        200_000
    );
    let launch = resolve("test-plain", Some(json!(500_000)));
    assert_eq!(launch.model_id.as_deref(), Some("test-plain"));
    assert!(launch.settings_json.is_none());
}

#[test]
fn boolean_options_reach_settings_only_when_supported() {
    let catalog = crate::claude_manifest::test_catalog();
    let both = [
        select("fastMode", json!(true)),
        select("thinking", json!(true)),
    ];
    let launch = ClaudeLaunchOptions::resolve_with(&catalog, Some("test-fixed"), &both);
    let fixed = settings(&launch);
    assert_eq!(fixed["fastMode"], true);
    assert!(fixed.get("alwaysThinkingEnabled").is_none());

    let launch = ClaudeLaunchOptions::resolve_with(&catalog, Some("test-plain"), &both);
    let plain = settings(&launch);
    assert_eq!(plain["alwaysThinkingEnabled"], true);
    assert!(plain.get("fastMode").is_none());
}

#[test]
fn todo_write_maps_to_plan_updated() {
    let mut m = Mapper::new();
    m.start_turn();
    let evs = feed(
        &mut m,
        r#"{"type":"assistant","message":{"id":"msg_t","content":[{"type":"tool_use","id":"toolu_todo","name":"TodoWrite","input":{"todos":[{"content":"Build board","status":"completed","activeForm":"Building board"},{"content":"","status":"in_progress"},{"content":"Ship","status":"todo"}]}}]}}"#,
    );
    assert_eq!(evs.len(), 1);
    match &evs[0] {
        AgentEvent::PlanUpdated { turn_id, steps, .. } => {
            assert_eq!(turn_id.as_deref(), Some("turn-1"));
            assert_eq!(steps.len(), 3);
            assert_eq!(steps[0].step, "Build board");
            assert_eq!(steps[0].status, PlanStepStatus::Completed);
            assert_eq!(steps[1].status, PlanStepStatus::InProgress);
            assert_eq!(steps[2].status, PlanStepStatus::Pending);
        }
        other => panic!("expected PlanUpdated, got {other:?}"),
    }

    let cleared = feed(
        &mut m,
        r#"{"type":"assistant","message":{"id":"msg_c","content":[{"type":"tool_use","id":"toolu_clear","name":"TodoWrite","input":{"todos":[]}}]}}"#,
    );
    assert!(matches!(
        cleared.as_slice(),
        [AgentEvent::PlanUpdated { steps, .. }] if steps.is_empty()
    ));
}

#[test]
fn extra_permission_mode_arg_updates_launch_tracker() {
    let extra_args = vec!["--permission-mode".into(), "plan".into()];
    assert_eq!(
        effective_permission_mode("default", &extra_args),
        "plan",
        "the last provider launch argument overrides the internal flag"
    );

    let extra_args = vec!["--permission-mode=bypassPermissions".into()];
    assert_eq!(
        effective_permission_mode("plan", &extra_args),
        "bypassPermissions"
    );
}

#[test]
fn user_message_carries_image_content_blocks() {
    let attachments = vec![
        Attachment {
            media_type: "image/png".into(),
            data_base64: "AAAA".into(),
            source_path: None,
        },
        Attachment {
            media_type: "image/jpeg".into(),
            data_base64: "BBBB".into(),
            source_path: None,
        },
    ];
    let msg = user_message("what color is this?", &attachments);
    let content = msg["message"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "what color is this?");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["type"], "base64");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert_eq!(content[1]["source"]["data"], "AAAA");
    assert_eq!(content[2]["source"]["media_type"], "image/jpeg");
    // Text-only stays a single text block.
    let plain = user_message("hi", &[]);
    assert_eq!(plain["message"]["content"].as_array().unwrap().len(), 1);
}

#[test]
fn init_parses_slash_commands_and_skills() {
    let mut m = Mapper::new();
    let evs = feed(
        &mut m,
        r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-4-8","slash_commands":["plan","review",""],"skills":["dataviz"]}"#,
    );
    assert!(
        matches!(evs.iter().find(|event| matches!(event, AgentEvent::SessionStarted { .. })),
            Some(AgentEvent::SessionStarted { provider_session_id, resume, model })
            if provider_session_id == "s1" && resume.0["session_id"] == "s1" && model.as_deref() == Some("claude-opus-4-8"))
    );
    let commands = evs
        .iter()
        .find_map(|event| match event {
            AgentEvent::ProviderCommands { commands } => Some(commands),
            _ => None,
        })
        .expect("init must publish native commands and skills");
    assert_eq!(
        commands
            .iter()
            .map(|command| (command.name.as_str(), command.kind))
            .collect::<Vec<_>>(),
        vec![
            ("plan", ProviderCommandKind::Command),
            ("review", ProviderCommandKind::Command),
            ("dataviz", ProviderCommandKind::Skill),
        ]
    );
    let repeated = feed(
        &mut m,
        r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-4-8"}"#,
    );
    assert!(!repeated.iter().any(|event| matches!(
        event,
        AgentEvent::SessionStarted { .. } | AgentEvent::ProviderCommands { .. }
    )));
}

#[test]
fn recorded_compaction_retains_metadata_without_claiming_post_context() {
    let mut m = Mapper::new();
    let mut events = Vec::new();
    for line in include_str!("../../tests/fixtures/claude/compaction_recorded.jsonl").lines() {
        events.extend(feed(&mut m, line));
    }
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], AgentEvent::ContextCompacted(c) if c.in_progress));
    assert!(matches!(&events[1], AgentEvent::ContextCompacted(c)
                if !c.in_progress && c.trigger.as_deref() == Some("manual")
                    && c.pre_tokens == Some(19555) && c.post_tokens == Some(2948)
                    && c.dropped_tokens == Some(16607) && c.duration_ms == Some(24455)));
}

#[test]
fn recorded_multi_request_usage_keeps_latest_context() {
    let mut m = Mapper::new();
    m.start_turn();
    let mut observations = Vec::new();
    let mut completion = None;
    for line in include_str!("../../tests/fixtures/claude/usage_recorded.jsonl").lines() {
        for event in feed(&mut m, line) {
            match event {
                AgentEvent::TokenUsage(u) => observations.push(u.used_tokens),
                AgentEvent::TurnCompleted { usage, .. } => completion = usage,
                _ => {}
            }
        }
    }
    assert_eq!(
        observations,
        vec![
            Some(20250),
            Some(20250),
            Some(20250),
            Some(20250),
            Some(20635),
            Some(20635),
            Some(20635),
            Some(20764),
            Some(20764),
            Some(20764)
        ]
    );
    let usage = completion.unwrap();
    assert_eq!(usage.used_tokens, Some(20764));
    assert_eq!(usage.output_tokens, Some(5));
    assert_eq!(usage.turn_processed_tokens, Some(62084));
    assert_eq!(usage.context_window, Some(1000000));
}

#[test]
fn streaming_usage_keeps_request_scope_at_completion() {
    let mut m = Mapper::new();
    m.start_turn();
    let start = feed(
        &mut m,
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"usage-1","usage":{"input_tokens":100,"cache_read_input_tokens":400,"cache_creation_input_tokens":50,"output_tokens":0}}}}"#,
    );
    assert!(
        matches!(start.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens == Some(550)),
        "message start must expose the measured request context: {start:?}"
    );
    let delta = feed(
        &mut m,
        r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":20}}}"#,
    );
    assert!(
        matches!(delta.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens == Some(550)),
        "output-only delta must retain input and cache: {delta:?}"
    );
    let result = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":1000,"cache_read_input_tokens":4000000,"output_tokens":200},"modelUsage":{"claude":{"contextWindow":1000000}}}"#,
    );
    assert!(result.iter().any(|e| matches!(e, AgentEvent::TurnCompleted { usage: Some(u), .. } if u.used_tokens == Some(550) && u.turn_processed_tokens == Some(4001200))), "result accounting must not replace occupancy: {result:?}");
    m.start_turn();
    let missing_start = feed(
        &mut m,
        r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":1}}}"#,
    );
    assert!(
        missing_start.is_empty(),
        "a new turn cannot reuse the previous request identity: {missing_start:?}"
    );
    let unknown = feed(
        &mut m,
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"usage-2"}}}"#,
    );
    assert!(matches!(unknown.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens.is_none()));
    let output_only = feed(
        &mut m,
        r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"output_tokens":1}}}"#,
    );
    assert!(
        matches!(output_only.last(), Some(AgentEvent::TokenUsage(u)) if u.used_tokens.is_none())
    );
}

#[test]
fn rejected_rate_limit_is_emitted_after_failed_result_error() {
    let mut m = Mapper::new();
    feed(
        &mut m,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1800000000,"rateLimitType":"five_hour","utilization":1.0}}"#,
    );

    let events = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"sess-1"}"#,
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::UsageLimitReached { .. }))
    );

    let mut m = Mapper::new();
    assert!(feed(
            &mut m,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1800000000,"rateLimitType":"five_hour","utilization":1.0}}"#,
        )
        .is_empty());

    let events = feed(
        &mut m,
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"You've hit your limit","session_id":"sess-1"}"#,
    );
    let error_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::Error { .. }))
        .expect("failed result error");
    assert!(matches!(
        events.get(error_index + 1),
        Some(AgentEvent::UsageLimitReached {
            resets_at: 1_800_000_000
        })
    ));
}

#[test]
fn streamed_deltas_preserve_block_identity_and_content_kind() {
    for (index, delta, expected_kind, expected_text) in [
        (
            0,
            json!({"type":"text_delta","text":"Hi"}),
            DeltaKind::AssistantText,
            "Hi",
        ),
        (
            1,
            json!({"type":"thinking_delta","thinking":"hmm"}),
            DeltaKind::ReasoningText,
            "hmm",
        ),
    ] {
        let mut m = Mapper::new();
        feed(
            &mut m,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1"}}}"#,
        );
        let events = feed(
            &mut m,
            &json!({"type":"stream_event", "event": {
                "type":"content_block_delta", "index":index, "delta":delta
            }})
            .to_string(),
        );
        assert!(
            matches!(events.as_slice(), [AgentEvent::Delta { item_id, kind, text }]
                if item_id == &format!("msg_1:{index}") && *kind == expected_kind && text == expected_text)
        );
    }
}

/// Captured from Claude Code 2.1.312 with claude-opus-5-5 and thinking on:
/// the prose the model wrote between two tool calls arrives as a second
/// thinking block holding the server's summary of it, and only the
/// `assistant` frame's `narration_block_indexes` tells it apart from
/// reasoning. It must complete the streamed item as a summarized message,
/// or the reply stays folded into the work log.
#[test]
fn narration_completes_its_streamed_block_as_a_summarized_message() {
    let mut m = Mapper::new();
    for line in [
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-5-5","id":"msg_n","content":[]}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}},"thinking_display":"updates"}"#,
        r#"{"type":"assistant","message":{"model":"claude-opus-5-5","id":"msg_n","content":[{"type":"thinking","thinking":"","signature":"CAQS+QQKEAgTGAI4AUIIdGhpbmtpbmc"}]},"thinking_duration_ms":2214,"thinking_display":"updates"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"","signature":""}},"thinking_display":"updates"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"f.txt contains two lines."}},"thinking_display":"updates"}"#,
    ] {
        feed(&mut m, line);
    }
    let completed = feed(
        &mut m,
        r#"{"type":"assistant","message":{"model":"claude-opus-5-5","id":"msg_n","content":[{"type":"thinking","thinking":"f.txt contains two lines.","signature":"CAQS0gYKEQgTGAI4AUIJbmFycmF0aW9u"}]},"narration_block_indexes":[0],"thinking_duration_ms":2,"thinking_display":"updates"}"#,
    );
    assert!(
        matches!(
            completed.as_slice(),
            [AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::AssistantMessage { text, summarized: true },
            })] if id == "msg_n:1" && text == "f.txt contains two lines."
        ),
        "{completed:?}"
    );
}

/// Captured from Claude Code 2.1.294 with claude-opus-5-5: the thinking
/// block opens with `content_block_start`, every `thinking_delta` is empty
/// and the `assistant` line repeats the block with no text. The block's
/// start is the one event that can announce the thinking, so it must
/// become an in-progress reasoning item that the empty deltas and the
/// empty completion leave alone.
#[test]
fn hidden_thinking_announces_itself_from_the_block_start() {
    let mut m = Mapper::new();
    feed(
        &mut m,
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_h"}}}"#,
    );
    let started = feed(
        &mut m,
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}}"#,
    );
    assert!(
        matches!(
            started.as_slice(),
            [AgentEvent::ItemStarted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::Reasoning { text },
            })] if id == "msg_h:0" && text.is_empty()
        ),
        "{started:?}"
    );
    for line in [
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}}"#,
        r#"{"type":"assistant","message":{"id":"msg_h","content":[{"type":"thinking","thinking":"","signature":"sig"}]}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}}"#,
    ] {
        let events = feed(&mut m, line);
        assert!(events.is_empty(), "{line}: {events:?}");
    }
}

/// Captured from a real session: the CLI splits one message across several
/// `assistant` lines, each carrying a single-element `content` array — a
/// (redacted, empty) thinking block first, then the text block. Enumerating
/// each array on its own numbered the text block 0 while its deltas streamed
/// under index 1, so the timeline rendered the paragraph twice: once live,
/// once again from the completion. The completed item must land on the
/// stream's id, and the empty thinking block must not complete an item.
#[test]
fn split_assistant_lines_keep_the_streams_block_numbering() {
    let mut m = Mapper::new();
    feed(
        &mut m,
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_s"}}}"#,
    );
    let streamed = feed(
        &mut m,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Added percent_off."}}}"#,
    );
    let streamed_id = match &streamed[0] {
        AgentEvent::Delta { item_id, .. } => item_id.clone(),
        other => panic!("expected Delta, got {other:?}"),
    };
    assert_eq!(streamed_id, "msg_s:1");

    // First `assistant` line: the thinking block alone, with no content.
    let thinking = feed(
        &mut m,
        r#"{"type":"assistant","message":{"id":"msg_s","content":[{"type":"thinking","thinking":""}]}}"#,
    );
    assert!(
        thinking.is_empty(),
        "an empty thinking block must not blank the streamed reasoning: {thinking:?}"
    );

    // Second `assistant` line: the text block, which the stream called 1.
    let completed = feed(
        &mut m,
        r#"{"type":"assistant","message":{"id":"msg_s","content":[{"type":"text","text":"Added percent_off."}]}}"#,
    );
    match &completed[0] {
        AgentEvent::ItemCompleted(item) => {
            assert_eq!(
                item.id, streamed_id,
                "completion must reuse the streamed item id, or the text renders twice"
            );
        }
        other => panic!("expected ItemCompleted, got {other:?}"),
    }
}

/// The shape Claude Code 2.1.316 prints (`claude -p --output-format
/// stream-json --verbose --model haiku` in a scratch directory holding an
/// 8x8 blue PNG and `a.txt`), its ids and paths invented: a Read of the
/// image, then a subagent that
/// runs Bash, reads and edits `a.txt`. The image is the result's only
/// content and reaches `image_reads` alone; the subagent's Bash and Edit
/// map as they do at the top level.
#[test]
fn recorded_image_read_and_subagent_tools_map_like_top_level_calls() {
    let mut mapper = Mapper::new();
    let mut completed = Vec::new();
    for line in include_str!("../../tests/fixtures/claude/tool_results_recorded.jsonl").lines() {
        for event in feed(&mut mapper, line) {
            if let AgentEvent::ItemCompleted(item) = event {
                completed.push(item);
            }
        }
    }
    let item = |id: &str| {
        completed
            .iter()
            .find(|item| item.id == id)
            .unwrap_or_else(|| panic!("{id} never completed: {completed:#?}"))
    };
    assert!(matches!(
        &item("toolu_010000000000000000000014").content,
        ItemContent::ToolCall { image_reads, family: Some(family), .. }
            if matches!(image_reads.as_slice(), [image] if image.media_type == "image/png")
                && matches!(&**family, ToolFamily::Read { path, .. } if path == "blue.png")
    ));
    let child = |id: &str| {
        let item = item(&format!("toolu_010000000000000000000015:{id}"));
        assert_eq!(
            item.parent_item_id.as_deref(),
            Some("toolu_010000000000000000000015")
        );
        &item.content
    };
    assert!(matches!(
        child("toolu_010000000000000000000016"),
        ItemContent::CommandExecution { command, .. } if command == "echo hi"
    ));
    assert_eq!(
            child("toolu_010000000000000000000018"),
            &ItemContent::FileChange {
                changes: vec![FileChange {
                    path: "/home/user/project/a.txt".into(),
                    kind: FileChangeKind::Modify,
                    diff: Some("-hello\n+bye".into()),
                }],
                status: ItemStatus::Completed,
                output: Some("The file /home/user/project/a.txt has been updated successfully. (file state is current in your context — no need to Read it back)".into()),
                command: None,
            }
        );
    // A subagent's tool result comes without the output object, so its
    // read is told by the lines the result numbers.
    assert!(matches!(
        child("toolu_010000000000000000000017"),
        ItemContent::ToolCall { name, output: Some(output), family: Some(family), .. }
            if name == "Read" && output == "hello\nworld\n" && matches!(&**family, ToolFamily::Read { path, first_line: Some(1), .. } if path == "/home/user/project/a.txt")
    ));
}

/// A command is a command whichever shell runs it, and one that exited
/// non-zero carries the code Claude Code wrote before its output
/// (2.1.322's tool error formatter: `Exit code ${code}`, then what the
/// command printed); the lines are stream-json `assistant` and `user`
/// messages with Anthropic `tool_use` and `tool_result` blocks, content
/// invented.
#[test]
fn a_command_carries_the_code_it_exited_with() {
    let mut mapper = Mapper::new();
    let started = feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg_ps","content":[{"type":"tool_use","id":"toolu_ps","name":"PowerShell","input":{"command":"Get-ChildItem build"}}]}}"#,
    );
    assert!(
        matches!(
            started.as_slice(),
            [AgentEvent::ItemStarted(ThreadItem { content: ItemContent::CommandExecution { command, .. }, .. })]
                if command == "Get-ChildItem build"
        ),
        "{started:?}"
    );
    let completed = feed(
        &mut mapper,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ps","is_error":true,"content":"Exit code 2\nGet-ChildItem: Cannot find path 'build'."}]}}"#,
    );
    let [AgentEvent::ItemCompleted(item)] = completed.as_slice() else {
        panic!("{completed:?}");
    };
    assert_eq!(
        item.content,
        ItemContent::CommandExecution {
            command: "Get-ChildItem build".into(),
            input: Vec::new(),
            output: "Get-ChildItem: Cannot find path 'build'.".into(),
            exit_code: Some(2),
            status: ItemStatus::Failed,
        }
    );
}

/// A Bash result's `bashEditDiff` names the files the command changed:
/// `files` with hunks and `created`/`deleted`, and `changedFiles` for
/// those it gives no diff (the Bash `tool_response` table of the hooks
/// reference, as Claude Code 2.1.323 writes it on the stream; content
/// invented). They follow the command as one file change that names it.
#[test]
fn the_files_a_command_changed_follow_it_as_one_file_change() {
    let mut mapper = Mapper::new();
    feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg_sed","content":[{"type":"tool_use","id":"toolu_sed","name":"Bash","input":{"command":"./regenerate.sh"}}]}}"#,
    );
    let completed = feed(
        &mut mapper,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_sed","content":"","is_error":false}]},"tool_use_result":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"bashEditDiff":{"files":[{"filePath":"/work/src/lib.rs","hunks":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":2,"lines":[" one","+two"]}]},{"filePath":"/work/new.rs","hunks":[{"oldStart":0,"oldLines":0,"newStart":1,"newLines":1,"lines":["+new"]}],"created":true},{"filePath":"/work/old.rs","hunks":[{"oldStart":1,"oldLines":1,"newStart":0,"newLines":0,"lines":["-old"]}],"deleted":true}],"moreFiles":1,"changedFiles":["/work/src/lib.rs","/work/new.rs","/work/old.rs","/work/generated.rs"]}}}"#,
    );
    let [
        AgentEvent::ItemCompleted(ThreadItem {
            id: command_id,
            content: ItemContent::CommandExecution { .. },
            ..
        }),
        AgentEvent::ItemCompleted(changed),
    ] = completed.as_slice()
    else {
        panic!("{completed:?}");
    };
    let change = |path: &str, kind, diff: Option<&str>| FileChange {
        path: path.into(),
        kind,
        diff: diff.map(str::to_owned),
    };
    assert_eq!(
        changed.content,
        ItemContent::FileChange {
            changes: vec![
                change(
                    "/work/src/lib.rs",
                    FileChangeKind::Modify,
                    Some("@@ -1,1 +1,2 @@\n one\n+two"),
                ),
                change(
                    "/work/new.rs",
                    FileChangeKind::Create,
                    Some("@@ -0,0 +1,1 @@\n+new"),
                ),
                change(
                    "/work/old.rs",
                    FileChangeKind::Delete,
                    Some("@@ -1,1 +0,0 @@\n-old"),
                ),
                change("/work/generated.rs", FileChangeKind::Changed, None),
            ],
            status: ItemStatus::Completed,
            output: None,
            command: Some(command_id.clone()),
        }
    );
    assert_ne!(&changed.id, command_id);
}

/// A Read is a read of the file it names: its lines as the output object
/// gives them, without the `N\t` numbers of the tool result, from the
/// line the read began at (`FileReadOutput` of `sdk-tools.d.ts` in
/// `@anthropic-ai/claude-agent-sdk` 0.3.322, for Claude Code 2.1.322;
/// content invented).
#[test]
fn a_read_is_the_lines_it_read_from_where_it_began() {
    let mut mapper = Mapper::new();
    let started = feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg_read","content":[{"type":"tool_use","id":"toolu_read","name":"Read","input":{"file_path":"/work/src/lib.rs","offset":40,"limit":2}}]}}"#,
    );
    assert!(
        matches!(
            started.as_slice(),
            [AgentEvent::ItemStarted(ThreadItem { content: ItemContent::ToolCall { family: Some(family), .. }, .. })]
                if matches!(&**family, ToolFamily::Read { path, first_line: None, .. } if path == "/work/src/lib.rs")
        ),
        "{started:?}"
    );
    let completed = feed(
        &mut mapper,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_read","content":"40\tfn main() {\n41\t}"}]},"tool_use_result":{"type":"text","file":{"filePath":"/work/src/lib.rs","content":"fn main() {\n}","numLines":2,"startLine":40,"totalLines":90}}}"#,
    );
    let [AgentEvent::ItemCompleted(item)] = completed.as_slice() else {
        panic!("{completed:?}");
    };
    let ItemContent::ToolCall { output, family, .. } = &item.content else {
        panic!("{item:?}");
    };
    assert_eq!(output.as_deref(), Some("fn main() {\n}"));
    assert_eq!(
        family.as_deref(),
        Some(&ToolFamily::Read {
            path: "/work/src/lib.rs".into(),
            first_line: Some(40),
            note: None,
        })
    );
}

/// A Grep is a search of what its output object found: in content mode
/// rg's `path:N:text` lines, context lines left out; a Glob the files in
/// its `filenames` (`GrepOutput`, `GlobOutput` of `sdk-tools.d.ts` in
/// `@anthropic-ai/claude-agent-sdk` 0.3.322; content invented).
#[test]
fn a_search_is_what_its_output_object_found() {
    let mut mapper = Mapper::new();
    feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg_grep","content":[{"type":"tool_use","id":"toolu_grep","name":"Grep","input":{"pattern":"TODO","path":"src","output_mode":"content","-C":1}},{"type":"tool_use","id":"toolu_glob","name":"Glob","input":{"pattern":"**/*.rs"}}]}}"#,
    );
    let completed = feed(
        &mut mapper,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_grep","content":"src/a.rs-2-fn a() {\nsrc/a.rs:3:    // TODO: one\n--\nsrc/b.rs:7:// TODO: two"}]},"tool_use_result":{"mode":"content","numFiles":0,"filenames":[],"content":"src/a.rs-2-fn a() {\nsrc/a.rs:3:    // TODO: one\n--\nsrc/b.rs:7:// TODO: two","numLines":4}}"#,
    );
    let [AgentEvent::ItemCompleted(grep)] = completed.as_slice() else {
        panic!("{completed:?}");
    };
    let ItemContent::ToolCall { family, .. } = &grep.content else {
        panic!("{grep:?}");
    };
    let hit = |path: &str, line, text: &str| SearchHit {
        path: path.into(),
        line: Some(line),
        text: Some(text.into()),
    };
    assert_eq!(
        family.as_deref(),
        Some(&ToolFamily::Search {
            scope: SearchScope::Content,
            pattern: Some("TODO".into()),
            path: Some("src".into()),
            glob: None,
            hits: Some(vec![
                hit("src/a.rs", 3, "    // TODO: one"),
                hit("src/b.rs", 7, "// TODO: two"),
            ]),
        })
    );
    let completed = feed(
        &mut mapper,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_glob","content":"src/a.rs\nsrc/b.rs"}]},"tool_use_result":{"filenames":["src/a.rs","src/b.rs"],"durationMs":3,"numFiles":2,"truncated":false}}"#,
    );
    let [AgentEvent::ItemCompleted(glob)] = completed.as_slice() else {
        panic!("{completed:?}");
    };
    let ItemContent::ToolCall { family, .. } = &glob.content else {
        panic!("{glob:?}");
    };
    let Some(ToolFamily::Search { scope, hits, .. }) = family.as_deref() else {
        panic!("{family:?}");
    };
    assert_eq!(*scope, SearchScope::Files);
    assert_eq!(
        hits.as_deref()
            .map(|hits| hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>()),
        Some(vec!["src/a.rs", "src/b.rs"])
    );
}

/// An AskUserQuestion is the questions it asked and the options each
/// answer names: Claude Code joins a multi-select's labels with ", ",
/// so a label holding ", " stays whole, and words no option begins are
/// the user's own (`AskUserQuestionInput`, `AskUserQuestionOutput` of
/// `sdk-tools.d.ts` in `@anthropic-ai/claude-agent-sdk` 0.3.322; content
/// invented).
#[test]
fn a_question_is_what_it_asked_and_the_options_each_answer_names() {
    let mut mapper = Mapper::new();
    feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg_ask","content":[{"type":"tool_use","id":"toolu_ask","name":"AskUserQuestion","input":{"questions":[{"question":"Which colours?","header":"Colours","multiSelect":true,"options":[{"label":"Red, green","description":"Warm and cool"},{"label":"Blue","description":"Cool"}]},{"question":"Why?","header":"Why","multiSelect":false,"options":[{"label":"Taste","description":""},{"label":"Brand","description":""}]}]}}]}}"#,
    );
    let completed = feed(
        &mut mapper,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ask","content":"Your questions have been answered: \"Which colours?\"=\"Red, green, Blue\", \"Why?\"=\"It matches the logo\". You can now continue with these answers in mind."}]},"tool_use_result":{"questions":[],"answers":{"Which colours?":"Red, green, Blue","Why?":"It matches the logo"}}}"#,
    );
    let [AgentEvent::ItemCompleted(item)] = completed.as_slice() else {
        panic!("{completed:?}");
    };
    let ItemContent::ToolCall { family, .. } = &item.content else {
        panic!("{item:?}");
    };
    let Some(ToolFamily::Question { questions }) = family.as_deref() else {
        panic!("{family:?}");
    };
    let answers: Vec<_> = questions
        .iter()
        .map(|question| question.answers.clone())
        .collect();
    assert_eq!(
        answers,
        [
            vec!["Red, green".to_owned(), "Blue".to_owned()],
            vec!["It matches the logo".to_owned()],
        ]
    );
}

/// A WebSearch is the pages its output object's searches found, past
/// the model's own words among them; a WebFetch the page and the prompt
/// it was read with (`WebSearchOutput`, `WebFetchInput` of
/// `sdk-tools.d.ts` in `@anthropic-ai/claude-agent-sdk` 0.3.322; content
/// invented).
#[test]
fn a_web_call_is_the_page_or_the_pages_it_found() {
    let mut mapper = Mapper::new();
    let started = feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg_web","content":[{"type":"tool_use","id":"toolu_search","name":"WebSearch","input":{"query":"cors preflight"}},{"type":"tool_use","id":"toolu_fetch","name":"WebFetch","input":{"url":"https://example.com/cors","prompt":"How long is it cached?"}}]}}"#,
    );
    assert!(
        matches!(
            &started[1],
            AgentEvent::ItemStarted(ThreadItem { content: ItemContent::ToolCall { family: Some(family), .. }, .. })
                if **family == ToolFamily::Fetch { url: "https://example.com/cors".into(), prompt: Some("How long is it cached?".into()) }
        ),
        "{started:?}"
    );
    let completed = feed(
        &mut mapper,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_search","content":"Web search results for query: \"cors preflight\"\n\nLinks: [{\"title\":\"CORS\",\"url\":\"https://example.com/cors\"}]\n\nREMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks."}]},"tool_use_result":{"query":"cors preflight","results":["Searching for the cache header.",{"tool_use_id":"srvtoolu_1","content":[{"title":"CORS","url":"https://example.com/cors"}]}],"durationSeconds":1.5}}"#,
    );
    let [AgentEvent::ItemCompleted(item)] = completed.as_slice() else {
        panic!("{completed:?}");
    };
    let ItemContent::ToolCall { family, .. } = &item.content else {
        panic!("{item:?}");
    };
    assert_eq!(
        family.as_deref(),
        Some(&ToolFamily::WebSearch {
            query: "cors preflight".into(),
            links: Some(vec![WebLink {
                title: "CORS".into(),
                url: "https://example.com/cors".into(),
            }]),
        })
    );
}

/// A ToolSearch is the tools its output object says it loaded
/// (`matches`), a Skill the skill it names (2.1.322's own schemas, not
/// in `sdk-tools.d.ts`; content invented).
#[test]
fn a_tool_search_is_the_tools_it_loaded_and_a_skill_its_name() {
    let mut mapper = Mapper::new();
    feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg_tools","content":[{"type":"tool_use","id":"toolu_tools","name":"ToolSearch","input":{"query":"select:Monitor","max_results":1}},{"type":"tool_use","id":"toolu_skill","name":"Skill","input":{"skill":"pr"}}]}}"#,
    );
    let completed = feed(
        &mut mapper,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_tools","content":[{"type":"tool_reference","tool_name":"Monitor"}]},{"type":"tool_result","tool_use_id":"toolu_skill","content":"Launching skill: pr"}]},"tool_use_result":{"matches":["Monitor"],"query":"select:Monitor","total_deferred_tools":12}}"#,
    );
    let families: Vec<_> = completed
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::ToolCall { family, .. },
                ..
            }) => family.as_deref().cloned(),
            _ => None,
        })
        .collect();
    assert_eq!(
        families,
        [
            ToolFamily::LoadTools {
                query: "select:Monitor".into(),
                tools: Some(vec!["Monitor".into()]),
            },
            ToolFamily::Skill { name: "pr".into() },
        ]
    );
}

/// A message sent to a resumed session whose background command did not
/// outlive the previous process: Claude Code first reports the command
/// stopped and closes that notification with a `result` of its own
/// (`num_turns: 0`, origin `task-notification`), then runs the message
/// (2.1.321's shape, its ids invented).
#[test]
fn a_resumed_sessions_stopped_task_result_leaves_the_message_turn_open() {
    let mut m = Mapper::new();
    let turn_id = m.start_turn();
    let turn_events: Vec<_> =
        include_str!("../../tests/fixtures/claude/resume_stopped_task_trace.jsonl")
            .lines()
            .flat_map(|line| feed(&mut m, line))
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::TurnStarted { .. }
                        | AgentEvent::TurnCompleted { .. }
                        | AgentEvent::ItemCompleted(ThreadItem {
                            content: ItemContent::AssistantMessage { .. },
                            ..
                        })
                )
            })
            .collect();
    assert!(
        matches!(
            turn_events.as_slice(),
            [
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::AssistantMessage { text, .. },
                    ..
                }),
                AgentEvent::TurnCompleted {
                    turn_id: completed,
                    status: TurnStatus::Completed,
                    ..
                },
            ] if text == "PONG" && completed == &turn_id
        ),
        "{turn_events:?}"
    );
}

#[test]
fn reinvocation_after_background_task_synthesizes_turn() {
    let mut m = Mapper::new();
    feed(
        &mut m,
        r#"{"type":"system","subtype":"init","session_id":"session-bg","model":"claude-haiku-4-5-20251001"}"#,
    );
    let first_turn_id = m.start_turn();

    let started = feed(
        &mut m,
        r#"{"type":"assistant","message":{"id":"msg-bg","content":[{"type":"tool_use","id":"toolu-bg","name":"Bash","input":{"command":"sleep 8 && echo BG_DONE","description":"Launch background sleep command","run_in_background":true}}]}}"#,
    );
    assert!(matches!(
        started.as_slice(),
        [AgentEvent::ItemStarted(ThreadItem {
            content: ItemContent::CommandExecution {
                status: ItemStatus::InProgress,
                ..
            },
            ..
        })]
    ));
    let count = feed_count(
        &mut m,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-1","task_type":"local_bash","description":"Launch background sleep command"}]}"#,
    );
    assert!(matches!(
        count.as_slice(),
        [AgentEvent::BackgroundTasksChanged { count: 1 }]
    ));
    let task_started = feed(
        &mut m,
        r#"{"type":"system","subtype":"task_started","task_id":"bg-1","tool_use_id":"toolu-bg","description":"Launch background sleep command","task_type":"local_bash"}"#,
    );
    // Claude lists the task before it names the call that started it;
    // the list follows once it has.
    assert!(matches!(
        task_started.as_slice(),
        [AgentEvent::BackgroundTaskList { tasks }]
            if tasks.len() == 1
                && tasks[0].item_id.as_deref() == Some("toolu-bg")
                && tasks[0].kind == crate::BackgroundKind::Command
    ));
    let running = feed(
        &mut m,
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu-bg","type":"tool_result","content":"Command running in background with ID: bg-1","is_error":false}]},"tool_use_result":{"stdout":"","stderr":"","interrupted":false,"backgroundTaskId":"bg-1"}}"#,
    );
    // The call returned; the command goes on in the background.
    assert!(matches!(
        running.as_slice(),
        [
            AgentEvent::ItemUpdated(ThreadItem {
                content: ItemContent::CommandExecution {
                    status: ItemStatus::InProgress,
                    exit_code: None,
                    ..
                },
                ..
            }),
            AgentEvent::ItemBackgrounded { item_id, parent_item_id: None },
        ] if item_id == "toolu-bg"
    ));
    let first_result = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Background command launched."}"#,
    );
    assert!(matches!(
        turn_completed_after_count(&first_result, 1),
        AgentEvent::TurnCompleted {
            turn_id,
            status: TurnStatus::Completed,
            ..
        } if turn_id == &first_turn_id
    ));

    assert!(
        feed_count(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
        )
        .is_empty()
    );
    assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"task_updated","task_id":"bg-1","patch":{"status":"completed","end_time":1784396058581}}"#,
            )
            .is_empty()
        );
    let notification = feed(
        &mut m,
        r#"{"type":"system","subtype":"task_notification","task_id":"bg-1","tool_use_id":"toolu-bg","status":"completed","output_file":"/tmp/bg-1.output","summary":"Background command completed (exit code 0)"}"#,
    );
    assert_eq!(notification.len(), 1);
    assert!(matches!(
        &notification[0],
        AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::CommandExecution {
                status: ItemStatus::Completed,
                exit_code: Some(0),
                ..
            },
            ..
        })
    ));
    let reinit = feed(
        &mut m,
        r#"{"type":"system","subtype":"init","session_id":"session-bg","model":"claude-haiku-4-5-20251001"}"#,
    );
    assert!(!reinit.iter().any(|event| matches!(
        event,
        AgentEvent::SessionStarted { .. }
            | AgentEvent::TurnStarted { .. }
            | AgentEvent::TurnCompleted { .. }
    )));

    let reinvoked = feed(
        &mut m,
        r#"{"type":"assistant","message":{"id":"msg-reinvoked","content":[{"type":"text","text":"Background command completed successfully."}]}}"#,
    );
    let synthesized_turn_id = match &reinvoked[0] {
        AgentEvent::TurnStarted { turn_id } => turn_id.clone(),
        other => panic!("expected synthesized TurnStarted first, got {other:?}"),
    };
    assert_ne!(synthesized_turn_id, first_turn_id);
    assert!(matches!(
        &reinvoked[1],
        AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::AssistantMessage { text, .. },
            ..
        }) if text == "Background command completed successfully."
    ));

    let second_result = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Background command completed successfully.","origin":{"kind":"task-notification"}}"#,
    );
    assert!(matches!(
        &second_result[0],
        AgentEvent::BackgroundTasksChanged { count: 0 }
    ));
    assert!(matches!(
        &second_result[1],
        AgentEvent::TurnCompleted {
            turn_id,
            status: TurnStatus::Completed,
            ..
        } if turn_id == &synthesized_turn_id
    ));
}

/// The zero-count publication must not depend on the CLI's `origin.kind`
/// shape: after `background_tasks_changed []` (whose zero is deliberately
/// withheld), ANY later result must tell the runtime the tasks are gone —
/// otherwise the session sits at "Working" forever on a live idle process.
#[test]
fn any_result_publishes_zero_after_background_tasks_drain() {
    let mut m = Mapper::new();
    m.start_turn();
    feed_count(
        &mut m,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-1","task_type":"local_bash","description":"sleep"}]}"#,
    );
    let first = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","is_error":false}"#,
    );
    turn_completed_after_count(&first, 1);

    // The drain event withholds its zero (parking guard)...
    assert!(
        feed_count(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
        )
        .is_empty()
    );
    // ...so a later ordinary result — no `origin` at all — must publish it.
    m.start_turn();
    let second = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","is_error":false}"#,
    );
    turn_completed_after_count(&second, 0);
}

/// A live change sends the model first, because `set_model` validates it
/// against the API before answering, then the flags; only the last answer
/// reports it. Answers recorded from Claude Code 2.1.318.
#[test]
fn a_live_settings_change_sends_the_model_then_the_flags() {
    let mut m = Mapper::new();
    m.applied_settings = ClaudeLaunchOptions {
        model_id: Some("claude-opus-4-8".into()),
        effort: Some("max".into()),
        ..ClaudeLaunchOptions::default()
    };
    let target = ClaudeLaunchOptions {
        model_id: Some("claude-sonnet-4-6[1m]".into()),
        effort: Some("low".into()),
        fast_mode: true,
        ..ClaudeLaunchOptions::default()
    };

    let model = m.begin_settings(target.clone()).unwrap().unwrap();
    assert_eq!(
        model["request"],
        json!({"subtype": "set_model", "model": "claude-sonnet-4-6[1m]"})
    );
    let answer = format!(
        r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{}","response":{{"model":"claude-sonnet-4-6"}}}}}}"#,
        model["request_id"].as_str().unwrap()
    );
    assert!(feed(&mut m, &answer).is_empty());

    let flags = m.take_outgoing().remove(0);
    assert_eq!(
        flags["request"],
        json!({
            "subtype": "apply_flag_settings",
            "settings": {"effortLevel": "low", "fastMode": true},
        })
    );
    let answer = format!(
        r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{}"}}}}"#,
        flags["request_id"].as_str().unwrap()
    );
    assert!(matches!(
        feed(&mut m, &answer).as_slice(),
        [AgentEvent::SettingsApplied { applied: true }]
    ));
    assert_eq!(
        m.begin_settings(target).unwrap(),
        None,
        "a process already there is not asked again"
    );
}

/// A model the CLI cannot validate leaves the process on its old one, and
/// the host restarts for it; a setting the CLI cannot take live is not
/// sent at all.
#[test]
fn a_live_settings_change_it_cannot_make_takes_a_restart() {
    let mut m = Mapper::new();
    m.applied_settings.model_id = Some("claude-opus-4-8".into());
    let request = m
        .begin_settings(ClaudeLaunchOptions {
            model_id: Some("claude-sonnet-4-6".into()),
            ..ClaudeLaunchOptions::default()
        })
        .unwrap()
        .unwrap();
    let answer = format!(
        r#"{{"type":"control_response","response":{{"subtype":"error","request_id":"{}","error":"Unable to validate model","error_code":"check_failed","model":"claude-opus-4-8"}}}}"#,
        request["request_id"].as_str().unwrap()
    );
    assert!(matches!(
        feed(&mut m, &answer).as_slice(),
        [AgentEvent::SettingsApplied { applied: false }]
    ));

    for target in [
        ClaudeLaunchOptions {
            thinking: Some(false),
            ..ClaudeLaunchOptions::default()
        },
        ClaudeLaunchOptions {
            auto_compact_window: Some(500_000),
            ..ClaudeLaunchOptions::default()
        },
    ] {
        assert!(m.begin_settings(target).is_err());
    }
}

/// A model that opens the selected window by itself needs no compaction
/// limit, so switching to it does not take a restart; leaving one the
/// process was launched with does, because only a launch sets that limit.
#[test]
fn a_model_with_its_own_window_needs_no_restart_for_it() {
    let native = ClaudeLaunchOptions {
        model_id: Some("claude-sonnet-4-6".into()),
        auto_compact_window: Some(200_000),
        native_window: true,
        ..ClaudeLaunchOptions::default()
    };
    let mut m = Mapper::new();
    m.applied_settings.model_id = Some("claude-opus-4-8".into());
    assert!(m.begin_settings(native.clone()).unwrap().is_some());

    let mut launched = Mapper::new();
    launched.applied_settings = native;
    assert!(
        launched
            .begin_settings(ClaudeLaunchOptions {
                model_id: Some("claude-opus-4-8".into()),
                ..ClaudeLaunchOptions::default()
            })
            .is_err()
    );
}

/// The list names what runs, keeps when each was first seen, and empties
/// at once, ahead of the withheld count. `stop_task` takes one id, so all
/// of them is one request each. Shapes recorded from Claude Code 2.1.318,
/// a subagent and the shell it started.
#[test]
fn background_tasks_are_listed_and_stopped_one_request_each() {
    let tasks = |events: &[AgentEvent]| {
        events
            .iter()
            .find_map(|event| match event {
                AgentEvent::BackgroundTaskList { tasks } => Some(tasks.clone()),
                _ => None,
            })
            .expect("a task change publishes the list")
    };
    let mut m = Mapper::new();
    let first = tasks(&feed(
        &mut m,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a0000000000000011","run_id":"000000002-00000000","task_type":"local_agent","subagent_type":"general-purpose","description":"wait helper"}]}"#,
    ));
    assert_eq!(first[0].id, "a0000000000000011");
    assert_eq!(first[0].name, "wait helper");
    assert_eq!(first[0].kind, crate::BackgroundKind::Agent);
    let second = tasks(&feed(
        &mut m,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a0000000000000011","run_id":"000000002-00000000","task_type":"local_agent","subagent_type":"general-purpose","description":"wait helper"},{"task_id":"b00000003","run_id":"000000003-00000000","task_type":"local_bash","description":"wait","parent_task_id":"a0000000000000011"}]}"#,
    ));
    assert_eq!(second.len(), 2);
    assert_eq!(second[0], first[0]);

    let requests: Vec<Value> = m
        .stop_task_requests(None)
        .into_iter()
        .map(|message| message["request"].clone())
        .collect();
    assert_eq!(
        requests,
        [
            json!({"subtype": "stop_task", "task_id": "a0000000000000011"}),
            json!({"subtype": "stop_task", "task_id": "b00000003"}),
        ]
    );

    let drained = feed(
        &mut m,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
    );
    assert!(
        matches!(drained.as_slice(), [AgentEvent::BackgroundTaskList { tasks }] if tasks.is_empty())
    );
}

/// Moving a call to the background is Ctrl+B for that call: the
/// `background_tasks` control request naming it (Claude Code 2.1.319's
/// SDK schema). A refusal is the only word of one that did nothing.
#[test]
fn a_call_moves_to_the_background_and_a_refusal_says_so() {
    let mut m = Mapper::new();
    let request = m.move_to_background_request("toolu-bash".into());
    assert_eq!(
        request["request"],
        json!({"subtype": "background_tasks", "tool_use_id": "toolu-bash"})
    );
    let id = request["request_id"].as_str().unwrap().to_owned();
    let refused = m.on_message(json!({"type":"control_response","response":{
            "subtype":"success","request_id": id,"response":{"backgrounded":false}}}));
    assert!(
        matches!(refused.as_slice(), [AgentEvent::Warning { .. }]),
        "{refused:?}"
    );

    let request = m.move_to_background_request("toolu-agent".into());
    let id = request["request_id"].as_str().unwrap().to_owned();
    let moved = m.on_message(json!({"type":"control_response","response":{
            "subtype":"success","request_id": id,"response":{"backgrounded":true}}}));
    assert!(moved.is_empty(), "{moved:?}");
}

/// A task stopped through `stop_task` starts no follow-up turn: the CLI
/// empties its list and goes idle, and that idle is the zero's only chance.
/// Shapes recorded from Claude Code 2.1.318.
#[test]
fn a_stopped_task_publishes_zero_when_the_cli_goes_idle() {
    let mut m = Mapper::new();
    feed_count(
        &mut m,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"b00000002","run_id":"000000004-00000000","task_type":"local_bash","description":"wait"}]}"#,
    );
    for line in [
        r#"{"type":"system","subtype":"task_updated","task_id":"b00000002","run_id":"000000004-00000000","patch":{"status":"killed","end_time":1794082805458}}"#,
        r#"{"type":"system","subtype":"task_notification","task_id":"b00000002","run_id":"000000004-00000000","tool_use_id":"toolu_0025","status":"stopped","summary":"wait"}"#,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
    ] {
        assert!(feed_count(&mut m, line).is_empty());
    }
    assert!(matches!(
        feed(
            &mut m,
            r#"{"type":"system","subtype":"session_state_changed","state":"idle"}"#,
        )
        .as_slice(),
        [AgentEvent::BackgroundTasksChanged { count: 0 }]
    ));
    assert!(
        feed(
            &mut m,
            r#"{"type":"system","subtype":"session_state_changed","state":"idle"}"#,
        )
        .is_empty(),
        "a turn's own idle says nothing about background work"
    );
}

/// Every task the CLI lists keeps the process busy, not only background
/// Bash: a backgrounded Agent (`task_type: local_agent`, as the CLI
/// reports it) must hold the count above zero until its notification's
/// follow-up result, or the runtime judges the session quiescent while
/// the subagent is still running. Shapes recorded from Claude Code 2.1.301.
#[test]
fn non_bash_background_tasks_count_until_their_follow_up_result() {
    let mut m = Mapper::new();
    m.start_turn();
    feed(
        &mut m,
        r#"{"type":"assistant","message":{"id":"msg-spawn","content":[{"type":"tool_use","id":"toolu_agent","name":"Agent","input":{"description":"Run sleep and echo command","subagent_type":"general-purpose","run_in_background":true}}]}}"#,
    );
    let counted = feed_count(
        &mut m,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a0000000000000012","run_id":"000000005-00000000","task_type":"local_agent","subagent_type":"general-purpose","description":"Run sleep and echo command"}]}"#,
    );
    assert!(matches!(
        counted.as_slice(),
        [AgentEvent::BackgroundTasksChanged { count: 1 }]
    ));
    feed(
        &mut m,
        r#"{"type":"system","subtype":"task_started","task_id":"a0000000000000012","tool_use_id":"toolu_agent","description":"Run sleep and echo command","subagent_type":"general-purpose","is_backgrounded":true,"task_type":"local_agent"}"#,
    );
    feed(
        &mut m,
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_agent","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]},"tool_use_result":{"isAsync":true,"status":"async_launched","agentId":"a0000000000000012"}}"#,
    );
    let launched = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"launched"}"#,
    );
    turn_completed_after_count(&launched, 1);

    // Between turns, the background subagent asks for permission.
    let asked = feed(
        &mut m,
        r#"{"type":"control_request","request_id":"ctrl-bg","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"cp a b"},"tool_use_id":"toolu_cp","agent_id":"a0000000000000012"}}"#,
    );
    let [AgentEvent::ApprovalRequested(request)] = asked.as_slice() else {
        panic!("expected one approval request, got {asked:?}");
    };
    assert_eq!(request.parent_item_id.as_deref(), Some("toolu_agent"));

    let notification = feed(
        &mut m,
        r#"{"type":"system","subtype":"task_notification","task_id":"a0000000000000012","tool_use_id":"toolu_agent","status":"completed","output_file":"/tmp/a0000000000000012.output","summary":"SUB_DONE"}"#,
    );
    assert!(
        !notification
            .iter()
            .any(|event| matches!(event, AgentEvent::ItemCompleted(_))),
        "a subagent task is settled as a Subagent, never as a command card: {notification:?}"
    );
    assert!(
        feed_count(
            &mut m,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
        )
        .is_empty()
    );
    m.start_turn();
    let done = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"DONE","origin":{"kind":"task-notification","producer":"session-task","runId":"000000005-00000000"}}"#,
    );
    turn_completed_after_count(&done, 0);
}

#[test]
fn unknown_task_notification_without_tool_use_id_does_not_wedge_turn() {
    let mut m = Mapper::new();
    let turn_id = m.start_turn();

    assert!(
            feed(
                &mut m,
                r#"{"type":"system","subtype":"task_notification","task_id":"unknown-task","status":"completed"}"#,
            )
            .is_empty()
        );
    let result = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","is_error":false}"#,
    );
    assert!(matches!(
        turn_completed_after_count(&result, 0),
        AgentEvent::TurnCompleted {
            turn_id: completed,
            status: TurnStatus::Completed,
            ..
        } if completed == &turn_id
    ));
}

/// A safety check that bypassPermissions cannot skip still asks; the
/// request carries Claude Code's consent line, and the agent's own
/// description of the command stays beside it.
#[test]
fn a_permission_request_says_why_claude_code_asks() {
    let mut m = Mapper::new();
    m.start_turn();
    let events = feed(
        &mut m,
        r#"{"type":"control_request","request_id":"ctrl-rm","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"rm -rf $F/*","description":"Clear the scratch directory"},"description":"Clear the scratch directory","decision_reason":"Dangerous rm operation detected in `rm -rf $F/*`. The target '$F/*' is a shell variable expansion: when $F is unset or empty it becomes `/`, `/*` or a top-level path. This requires explicit approval and cannot be auto-allowed by permission rules.","decision_reason_type":"safetyCheck","classifier_approvable":false,"tool_use_id":"toolu_rm"}}"#,
    );
    let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
        panic!("expected one approval request, got {events:?}");
    };
    assert_eq!(
        request.reason.as_deref(),
        Some(
            "Dangerous rm operation detected in `rm -rf $F/*`. The target '$F/*' is a shell variable expansion: when $F is unset or empty it becomes `/`, `/*` or a top-level path. This requires explicit approval and cannot be auto-allowed by permission rules."
        )
    );
    assert_eq!(request.blocked_path, None);
    assert_eq!(
        request.kind,
        ApprovalKind::ExecCommand {
            command: "rm -rf $F/*".into(),
            cwd: None,
            description: Some("Clear the scratch directory".into()),
        }
    );
}

#[test]
fn approval_details_reach_native_permission_requests() {
    let mut m = Mapper::new();
    m.start_turn();
    for (tool, input, expected) in [
        (
            "Task",
            json!({"subagent_type":"explore", "description":"find refs", "prompt":"ignored"}),
            Some("explore: find refs"),
        ),
        (
            "Task",
            json!({"prompt":"find references"}),
            Some("find references"),
        ),
        ("Weird", json!({"a":1}), Some("Weird: {\"a\":1}")),
        (
            "Read",
            json!({"file_path":"/tmp/a.txt"}),
            Some("Read: {\"file_path\":\"/tmp/a.txt\"}"),
        ),
        ("Task", json!({"prompt":"y".repeat(300)}), None),
        ("Weird", json!({"blob":"z".repeat(500)}), None),
    ] {
        let events = m.on_message(json!({
            "type": "control_request",
            "request_id": "detail-request",
            "request": {"subtype":"can_use_tool", "tool_name":tool, "input":input},
        }));
        let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
            panic!("expected native permission request, got {events:?}");
        };
        let detail = match &request.kind {
            ApprovalKind::ToolUse {
                name,
                input: actual_input,
                detail,
            } => {
                assert_eq!(name, tool);
                assert_eq!(actual_input, &input);
                detail
            }
            ApprovalKind::FileRead { detail } if tool == "Read" => detail,
            other => panic!("unexpected approval for {tool}: {other:?}"),
        };
        if let Some(expected) = expected {
            assert_eq!(detail, expected);
        } else if tool == "Task" {
            let prompt = input["prompt"].as_str().unwrap();
            assert!(!detail.is_empty());
            assert!(prompt.starts_with(detail.as_str()));
            assert!(
                detail.len() < prompt.len(),
                "long task prompts must remain bounded"
            );
        } else {
            assert!(detail.starts_with("Weird: {\"blob\":\""));
            assert!(detail.ends_with("..."));
            assert!(detail.len() < input["blob"].as_str().unwrap().len());
        }
    }
}

#[test]
fn exit_plan_mode_shows_its_plan_and_takes_the_decision_natively() {
    let plan = "# Plan\n\n1. Write hello.txt";
    let input = json!({"plan": plan, "planFilePath": "/plans/p.md"});
    for (choice, expected) in [
        (
            ApprovalDecision::Option("allow".into()),
            json!({"behavior": "allow", "updatedInput": input}),
        ),
        (
            ApprovalDecision::Option("deny".into()),
            json!({"behavior": "deny", "message": "User declined tool execution."}),
        ),
        (
            ApprovalDecision::Feedback("Use Python instead".into()),
            json!({"behavior": "deny", "message": "Use Python instead"}),
        ),
    ] {
        let mut mapper = Mapper::new();
        let started = mapper.on_message(json!({"type":"assistant", "message": {"id":"msg_plan",
                "content":[{"type":"tool_use", "id":"toolu_plan", "name":"ExitPlanMode", "input":input}]}}));
        assert!(
                started.iter().any(|event| matches!(event, AgentEvent::ItemStarted(ThreadItem { id, content: ItemContent::Plan { markdown }, .. })
                    if id == "toolu_plan" && markdown == plan)),
                "{started:?}"
            );
        let events = mapper.on_message(json!({"type":"control_request", "request_id":"plan-request",
                "request": {"subtype":"can_use_tool", "tool_name":"ExitPlanMode", "input":input, "tool_use_id":"toolu_plan"}}));
        let [AgentEvent::ApprovalRequested(request)] = events.as_slice() else {
            panic!("the plan item is already shown: {events:?}")
        };
        assert_eq!(
            request.kind,
            ApprovalKind::Plan {
                item_id: "toolu_plan".into(),
                markdown: plan.into()
            }
        );
        assert_eq!(
            request
                .options
                .iter()
                .map(|option| (option.id.as_str(), option.kind))
                .collect::<Vec<_>>(),
            [
                ("allow", ApprovalOptionKind::AllowOnce),
                ("deny", ApprovalOptionKind::RejectOnce)
            ]
        );
        let response = mapper
            .build_approval_response("plan-request", choice)
            .unwrap();
        assert_eq!(response["response"]["response"], expected);
        let completed = mapper.on_message(json!({"type":"user", "message": {"content":[
                {"type":"tool_result", "tool_use_id":"toolu_plan", "content":"User has approved your plan."}]}}));
        assert!(
                completed.iter().any(|event| matches!(event, AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::Plan { markdown }, .. })
                    if id == "toolu_plan" && markdown == plan)),
                "{completed:?}"
            );
    }

    let mut mapper = Mapper::new();
    let events = mapper.on_message(json!({"type":"control_request", "request_id":"early",
            "request": {"subtype":"can_use_tool", "tool_name":"ExitPlanMode", "input":{"plan": plan}, "tool_use_id":"toolu_early"}}));
    assert!(
        matches!(events.as_slice(), [
                AgentEvent::ItemStarted(ThreadItem { id, content: ItemContent::Plan { markdown }, .. }),
                AgentEvent::ApprovalRequested(ApprovalRequest { kind: ApprovalKind::Plan { item_id, .. }, .. }),
            ] if id == "toolu_early" && item_id == id && markdown == plan),
        "a prompt that arrives before its tool block still shows the plan it decides: {events:?}"
    );
}

/// `control_cancel_request {request_id}` as Claude Code 2.1.322's SDK
/// schema describes it: the sender no longer needs the answer to its own
/// request, "for example a pending can_use_tool prompt after the turn was
/// interrupted". Its `sendRequest` writes one when the turn's abort signal
/// fires.
#[test]
fn prompts_claude_code_withdraws_are_reported_resolved() {
    let mut m = Mapper::new();
    m.start_turn();
    feed(
        &mut m,
        r#"{"type":"control_request","request_id":"ctrl-bash","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"cargo test"},"tool_use_id":"toolu_bash"}}"#,
    );
    feed(
        &mut m,
        r#"{"type":"control_request","request_id":"ctrl-ask","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Which DB?","header":"DB"}]},"tool_use_id":"toolu_ask"}}"#,
    );
    let events = feed(
        &mut m,
        r#"{"type":"control_cancel_request","request_id":"ctrl-bash"}"#,
    );
    assert!(
        matches!(events.as_slice(), [AgentEvent::ApprovalResolved { request_id, decision: ApprovalDecision::Cancel }] if request_id == "ctrl-bash"),
        "{events:?}"
    );
    let events = feed(
        &mut m,
        r#"{"type":"control_cancel_request","request_id":"ctrl-ask"}"#,
    );
    assert!(
        matches!(events.as_slice(), [AgentEvent::UserInputResolved { request_id, answers }] if request_id == "ctrl-ask" && answers.is_empty()),
        "{events:?}"
    );
    assert!(
        m.build_approval_response("ctrl-bash", ApprovalDecision::Option("allow".into()))
            .is_none()
    );
    assert!(
        feed(
            &mut m,
            r#"{"type":"control_cancel_request","request_id":"ctrl-bash"}"#
        )
        .is_empty()
    );
}

#[test]
fn ask_user_question_parse_and_answer_wire_shape() {
    let mut m = Mapper::new();
    m.start_turn();
    let evs = feed(
        &mut m,
        r#"{"type":"control_request","request_id":"ctrl-9","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Which color?","header":"Color","options":[{"label":"Red","description":"warm"},{"label":"Blue","description":""}],"multiSelect":false},{"header":"Free"}]}}}"#,
    );
    assert_eq!(evs.len(), 1);
    match &evs[0] {
        AgentEvent::UserInputRequested {
            request_id,
            questions,
            ..
        } => {
            assert_eq!(request_id, "ctrl-9");
            assert_eq!(questions.len(), 2);
            // id = complete question text.
            assert_eq!(questions[0].id, "Which color?");
            assert_eq!(questions[0].header, "Color");
            assert_eq!(questions[0].options.len(), 2);
            assert_eq!(questions[0].options[0].label, "Red");
            assert_eq!(questions[0].options[1].description, "");
            assert!(!questions[0].multi_select);
            // Missing question text → id fallback q-<index>, header kept.
            assert_eq!(questions[1].id, "q-1");
            assert_eq!(questions[1].header, "Free");
            assert!(questions[1].options.is_empty());
        }
        other => panic!("expected UserInputRequested, got {other:?}"),
    }

    // Answer: allow with {questions: <original>, answers: <provided>}.
    let mut answers = serde_json::Map::new();
    answers.insert("Which color?".into(), json!("Red"));
    let resp = m
        .build_user_input_response("ctrl-9", &answers)
        .expect("response for known request");
    assert_eq!(resp["response"]["subtype"], "success");
    assert_eq!(resp["response"]["request_id"], "ctrl-9");
    assert_eq!(resp["response"]["response"]["behavior"], "allow");
    assert_eq!(
        resp["response"]["response"]["updatedInput"]["answers"]["Which color?"],
        "Red"
    );
    // Original questions echoed back verbatim.
    assert_eq!(
        resp["response"]["response"]["updatedInput"]["questions"][0]["header"],
        "Color"
    );
    // Consumed once.
    assert!(m.build_user_input_response("ctrl-9", &answers).is_none());
}

/// The `elicitation` control request and its `{action, content}` answer
/// are Claude Code 2.1.323's SDK control schema; the fields are MCP
/// 2025-11-25 `PrimitiveSchemaDefinition`s (schema.ts).
#[test]
fn an_mcp_form_is_asked_as_questions_with_its_defaults_and_answered_in_its_types() {
    let form = r#"{"type":"control_request","request_id":"REQUEST","request":{"subtype":"elicitation","mcp_server_name":"deployer","message":"Where should this go?","mode":"form","requested_schema":{"type":"object","properties":{"public":{"type":"boolean","title":"Public","default":true},"region":{"type":"string","title":"Region","description":"A region code","default":"eu-west"},"replicas":{"type":"integer","minimum":1},"tier":{"type":"string","title":"Tier","oneOf":[{"const":"std","title":"Standard"},{"const":"pro","title":"Professional"}],"default":"pro"},"zones":{"type":"array","title":"Zones","items":{"type":"string","enum":["a","b","c"]}}},"required":["region"]},"property_order":["region","replicas","public","tier","zones"]}}"#;
    let mut m = Mapper::new();
    m.start_turn();
    let events = feed(&mut m, &form.replace("REQUEST", "elicit-1"));
    let question = |id: &str, header: &str, question: &str, options: &[&str]| UserInputQuestion {
        id: id.into(),
        header: header.into(),
        question: question.into(),
        options: options
            .iter()
            .map(|label| UserInputOption {
                label: (*label).into(),
                description: String::new(),
            })
            .collect(),
        multi_select: id == "zones",
        prefill: match id {
            "region" => Some("eu-west".into()),
            "public" => Some("true".into()),
            "tier" => Some("Professional".into()),
            _ => None,
        },
        optional: id != "region",
    };
    let asked_by = "deployer: Where should this go?";
    let [
        AgentEvent::UserInputRequested {
            request_id,
            questions,
            delivery: UserInputDelivery::Blocking,
        },
    ] = events.as_slice()
    else {
        panic!("{events:?}");
    };
    assert_eq!(request_id, "elicit-1");
    assert_eq!(
        questions,
        &[
            question(
                "region",
                "Region",
                &format!("{asked_by}\n\nA region code"),
                &[]
            ),
            question("replicas", "replicas", asked_by, &[]),
            question("public", "Public", asked_by, &["true", "false"]),
            question("tier", "Tier", asked_by, &["Standard", "Professional"]),
            question("zones", "Zones", asked_by, &["a", "b", "c"]),
        ]
    );
    assert!(m.take_outgoing().is_empty());

    let answers = json!({
        "region": "us-east", "replicas": "3", "public": "true",
        "tier": "Professional", "zones": ["a", "c"],
    });
    assert_eq!(
        m.build_user_input_response("elicit-1", answers.as_object().unwrap()),
        Some(
            json!({"type":"control_response","response":{"subtype":"success","request_id":"elicit-1","response":{
                "action":"accept",
                "content":{"region":"us-east","replicas":3,"public":true,"tier":"pro","zones":["a","c"]},
            }}})
        )
    );

    feed(&mut m, &form.replace("REQUEST", "elicit-2"));
    assert_eq!(
        m.build_user_input_response("elicit-2", &serde_json::Map::new()),
        Some(
            json!({"type":"control_response","response":{"subtype":"success","request_id":"elicit-2","response":{"action":"cancel"}}})
        ),
        "dismissed without an answer"
    );

    // With no `required`, every field may be left out, and a form left
    // wholly empty is still accepted.
    let events = feed(
        &mut m,
        &form
            .replace("REQUEST", "elicit-3")
            .replace(r#","required":["region"]"#, ""),
    );
    let [AgentEvent::UserInputRequested { questions, .. }] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert!(questions.iter().all(|question| question.optional));
    let answers = json!({
        "region": "", "replicas": "", "public": "", "tier": "", "zones": [],
    });
    assert_eq!(
        m.build_user_input_response("elicit-3", answers.as_object().unwrap()),
        Some(
            json!({"type":"control_response","response":{"subtype":"success","request_id":"elicit-3","response":{"action":"accept","content":{}}}})
        )
    );
}

/// A URL-mode `elicitation` and `mcp_message` are requests of Claude Code
/// 2.1.323's SDK control schema that Tcode has no way to serve. The Agent
/// SDK of that version declines an elicitation it has no handler for and
/// answers the error "Unsupported control request subtype: …", code
/// `unsupported_request`, to a request it does not know (0.3.323,
/// `sdk.mjs`); a request with no answer holds its tool call open for good.
#[test]
fn a_control_request_tcode_cannot_serve_is_answered() {
    let mut m = Mapper::new();
    m.start_turn();
    let events = feed(
        &mut m,
        r#"{"type":"control_request","request_id":"elicit-url","request":{"subtype":"elicitation","mcp_server_name":"tracker","message":"Sign in to continue","mode":"url","url":"https://tracker.example/authorize","elicitation_id":"el-7"}}"#,
    );
    assert_eq!(
        m.take_outgoing(),
        [
            json!({"type":"control_response","response":{"subtype":"success","request_id":"elicit-url","response":{"action":"decline"}}})
        ]
    );
    assert!(
        matches!(events.as_slice(), [AgentEvent::Warning { message }]
                if ["tracker", "Sign in to continue", "https://tracker.example/authorize"]
                    .iter()
                    .all(|said| message.contains(said))),
        "the user is told who asked and what: {events:?}"
    );

    let events = feed(
        &mut m,
        r#"{"type":"control_request","request_id":"mcp-1","request":{"subtype":"mcp_message","server_name":"tracker","message":{"jsonrpc":"2.0","id":1,"method":"ping"}}}"#,
    );
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(
        m.take_outgoing(),
        [
            json!({"type":"control_response","response":{"subtype":"error","request_id":"mcp-1","error":"Unsupported control request subtype: mcp_message","error_code":"unsupported_request"}})
        ]
    );
}

#[test]
fn result_maps_to_turn_completed_with_usage() {
    let mut m = Mapper::new();
    let turn_id = m.start_turn();
    let evs = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.125,"duration_ms":4321,"usage":{"input_tokens":100,"cache_read_input_tokens":50,"cache_creation_input_tokens":10,"output_tokens":20},"modelUsage":{"claude-opus-4-8[1m]":{"contextWindow":1000000}}}"#,
    );
    match turn_completed_after_count(&evs, 0) {
        AgentEvent::TurnCompleted {
            turn_id: tid,
            status,
            usage,
        } => {
            assert_eq!(tid, &turn_id);
            assert_eq!(*status, TurnStatus::Completed);
            let usage = usage.as_ref().expect("usage present");
            assert_eq!(usage.input_tokens, None);
            assert_eq!(usage.cached_input_tokens, None);
            assert_eq!(usage.output_tokens, None);
            assert_eq!(usage.used_tokens, None);
            assert_eq!(usage.turn_processed_tokens, Some(180));
            assert_eq!(usage.total_processed_tokens, None);
            assert_eq!(usage.context_window, Some(1_000_000));
            assert_eq!(usage.cost_usd, Some(0.125));
            assert_eq!(usage.duration_ms, Some(4321));
        }
        other => panic!("expected TurnCompleted, got {other:?}"),
    }
    // Each result is that turn's traffic, however long the process ran.
    m.start_turn();
    let evs = feed(
        &mut m,
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":30,"output_tokens":5}}"#,
    );
    assert!(matches!(
        turn_completed_after_count(&evs, 0),
        AgentEvent::TurnCompleted { usage: Some(usage), .. }
            if usage.turn_processed_tokens == Some(35) && usage.total_processed_tokens.is_none()
    ));
}

#[test]
fn interrupted_result_status() {
    let mut idle = Mapper::new();
    idle.interrupt_request();
    let idle_result = feed(
        &mut idle,
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"provider failed"}"#,
    );
    // A failed turn discloses the CLI's raw result text before the marker.
    assert!(matches!(
        &idle_result[0],
        AgentEvent::Error { message, fatal: false }
            if message.contains("provider failed")
    ));
    assert!(matches!(
        turn_completed_after_count(&idle_result, 0),
        AgentEvent::TurnCompleted {
            status: TurnStatus::Failed,
            ..
        }
    ));

    let mut m = Mapper::new();
    m.start_turn();
    m.interrupt_request();
    let attributed = feed(
        &mut m,
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"provider failed"}"#,
    );
    assert!(matches!(
        turn_completed_after_count(&attributed, 0),
        AgentEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            ..
        }
    ));
}

#[test]
fn full_fixture_trace_parses() {
    let trace = include_str!("../../tests/fixtures/claude/tool_use_trace.jsonl");
    let mut m = Mapper::new();
    let mut all = Vec::new();
    let mut turn_id = None;
    let mut approvals = 0;
    for line in trace.lines().filter(|line| !line.trim().is_empty()) {
        let msg: Value = serde_json::from_str(line).expect("fixture line is json");
        for event in m.on_message(msg) {
            if let AgentEvent::TurnStarted { turn_id: started } = &event {
                turn_id = Some(started.clone());
            }
            if let AgentEvent::ApprovalRequested(request) = &event {
                approvals += 1;
                assert_eq!(request.id, "00000000-0000-4000-8000-000000000047");
                assert!(turn_id.is_some());
                assert_eq!(request.turn_id, turn_id);
                let ApprovalKind::FileChange { changes } = &request.kind else {
                    panic!("expected Write approval, got {request:?}");
                };
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].path, "/home/user/project/hello.txt");
                assert_eq!(
                    request.reason, None,
                    "the frame says what the tool does, not why it asks"
                );
                let response = m
                    .build_approval_response(&request.id, ApprovalDecision::Option("allow".into()))
                    .expect("response to the recorded request");
                assert_eq!(
                    response,
                    json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": "00000000-0000-4000-8000-000000000047",
                            "response": {
                                "behavior": "allow",
                                "updatedInput": {
                                    "file_path": "/home/user/project/hello.txt",
                                    "content": "hi\n",
                                },
                            },
                        },
                    })
                );
                assert!(
                    m.build_approval_response(
                        &request.id,
                        ApprovalDecision::Option("allow".into())
                    )
                    .is_none()
                );
            }
            all.push(event);
        }
    }
    assert!(
        all.iter()
            .any(|event| matches!(event, AgentEvent::SessionStarted { .. })),
        "expected SessionStarted"
    );
    assert_eq!(approvals, 1);

    // Empty structuredPatch in the recorded result must preserve the Write input's diff.
    let changes: Vec<_> = all
        .iter()
        .filter_map(|event| {
            let (phase, item) = match event {
                AgentEvent::ItemStarted(item) => ("started", item),
                AgentEvent::ItemCompleted(item) => ("completed", item),
                _ => return None,
            };
            let ItemContent::FileChange {
                changes, status, ..
            } = &item.content
            else {
                return None;
            };
            Some((phase, item.id.as_str(), *status, changes.as_slice()))
        })
        .collect();
    let expected = [FileChange {
        path: "/home/user/project/hello.txt".into(),
        kind: FileChangeKind::Create,
        diff: Some("+hi".into()),
    }];
    assert_eq!(
        changes,
        vec![
            (
                "started",
                "toolu_010000000000000000000019",
                ItemStatus::InProgress,
                expected.as_slice()
            ),
            (
                "completed",
                "toolu_010000000000000000000019",
                ItemStatus::Completed,
                expected.as_slice()
            ),
        ]
    );
    assert!(
        all.iter().any(|event| matches!(
            event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        )),
        "expected completed TurnCompleted"
    );
}

/// The snapshots of the Subagent item `id` in `events`.
fn subagent_snapshots<'a>(events: &'a [AgentEvent], id: &str) -> Vec<&'a ThreadItem> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item)
                if item.id == id && matches!(item.content, ItemContent::Subagent { .. }) =>
            {
                Some(item)
            }
            _ => None,
        })
        .collect()
}

/// A foreground subagent as Claude Code recorded it: the lead waits on
/// it, its activity is parented to the spawn, the spawn shows the model
/// the child ran with, and no terminal snapshot reaches the thread before
/// the transcript tail's last flush.
#[test]
fn subagent_fixture_maps_lifecycle_and_parented_activity() {
    const SPAWN: &str = "toolu_010000000000000000000007";
    let trace = include_str!("../../tests/fixtures/claude/subagent_handback_trace.jsonl");
    let mut mapper = Mapper::new();
    let mut events = Vec::new();
    for line in trace.lines() {
        events.extend(feed(&mut mapper, line));
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ItemBackgrounded { .. }))
    );
    assert!(
        subagent_snapshots(&events, SPAWN)
            .iter()
            .all(|item| matches!(
                item.content,
                ItemContent::Subagent {
                    status: ItemStatus::InProgress,
                    ..
                }
            )),
        "{events:?}"
    );
    let children: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ItemStarted(item) | AgentEvent::ItemCompleted(item)
                if item.parent_item_id.as_deref() == Some(SPAWN) =>
            {
                Some(&item.content)
            }
            _ => None,
        })
        .collect();
    assert!(
            children.iter().any(|content| matches!(content,
                ItemContent::UserMessage { text, .. } if text.starts_with("Your report is two lines"))),
            "{children:?}"
        );
    assert!(
            children.iter().any(|content| matches!(content,
                ItemContent::ToolCall { name, status: ItemStatus::Completed, .. } if name == "SubagentHandback")),
            "{children:?}"
        );

    let flushed = mapper.on_tail_notice(SubagentTailNotice {
        parent_id: SPAWN.into(),
        events: Vec::new(),
        model: None,
        stopped: true,
    });
    assert!(
        matches!(
            subagent_snapshots(&flushed, SPAWN).last(),
            Some(ThreadItem { parent_item_id: None, content: ItemContent::Subagent { status: ItemStatus::Completed, model: Some(model), .. }, .. })
                if model == "claude-sonnet-5-5"
        ),
        "{flushed:?}"
    );
}

/// A background subagent as Claude Code recorded it: the launch
/// acknowledgement returns the call and leaves the Subagent item in
/// progress; it settles only with the transcript tail's final flush, and
/// the tail's copy of a child the stdout feed already completed cannot
/// reopen it.
#[test]
fn background_subagent_terminal_waits_for_tail_flush_and_dedupes_feeds() {
    const SPAWN: &str = "toolu_010000000000000000000002";
    let trace =
        include_str!("../../tests/fixtures/claude/subagent_handback_background_trace.jsonl");
    let mut mapper = Mapper::new();
    let mut events = Vec::new();
    for line in trace.lines() {
        events.extend(feed(&mut mapper, line));
    }
    assert!(
        subagent_snapshots(&events, SPAWN)
            .iter()
            .all(|item| matches!(
                item.content,
                ItemContent::Subagent {
                    status: ItemStatus::InProgress,
                    ..
                }
            )),
        "launch acknowledgement and task_notification must not settle the item yet: {events:?}"
    );
    assert!(events.iter().any(|event| matches!(event,
            AgentEvent::ItemBackgrounded { item_id, parent_item_id: None } if item_id == SPAWN)));
    assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::ToolCall { status: ItemStatus::Completed, .. }, .. })
                if *id == format!("{SPAWN}:toolu_010000000000000000000003")
        )));

    // The tail maps the same child records a second time.
    let mut tail = crate::subagent_tail::TranscriptMapper::new(SPAWN);
    let child = format!("\"parent_tool_use_id\":\"{SPAWN}\"");
    let mut tail_events = Vec::new();
    for line in trace.lines().filter(|line| line.contains(&child)) {
        tail_events.extend(tail.map_value(&serde_json::from_str(line).unwrap()));
    }
    assert!(!tail_events.is_empty());
    let flushed = mapper.on_tail_notice(SubagentTailNotice {
        parent_id: SPAWN.into(),
        events: tail_events,
        model: None,
        stopped: true,
    });
    assert!(
        flushed
            .iter()
            .all(|event| !matches!(event, AgentEvent::ItemStarted(_))),
        "a completed child must not be restarted by the tail's copy: {flushed:?}"
    );
    assert!(
        matches!(
            flushed.last(),
            Some(AgentEvent::ItemUpdated(ThreadItem { id, content: ItemContent::Subagent { status: ItemStatus::Completed, .. }, .. }))
                if id == SPAWN
        ),
        "{flushed:?}"
    );
}

#[test]
fn resumed_subagent_keeps_its_spawn_identity_across_parent_restart() {
    smol::block_on(async {
        let root =
            std::env::temp_dir().join(format!("tcode-resumed-agent-{}", uuid::Uuid::new_v4()));
        let dir = root.join("projects/project/session/subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent-child.meta.json"),
                r#"{"agentType":"general-purpose","description":"Audit routing","toolUseId":"spawn","spawnDepth":1,"requestShape":"background","requestNonInteractive":true}"#
            ).unwrap();
        for restart in [true, false] {
            let mut mapper = Mapper::new();
            if !restart {
                feed(
                    &mut mapper,
                    r#"{"type":"assistant","message":{"id":"initial","content":[{"type":"tool_use","id":"spawn","name":"Agent","input":{"description":"Audit routing","subagent_type":"general-purpose"}}]}}"#,
                );
                feed(
                    &mut mapper,
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"spawn","content":"Launched"}]},"tool_use_result":{"status":"async_launched","agentId":"child"}}"#,
                );
                feed(
                    &mut mapper,
                    r#"{"type":"system","subtype":"task_notification","task_id":"child","tool_use_id":"spawn","status":"completed"}"#,
                );
            }
            let mut events = Vec::new();
            for msg in [
                json!({"type":"assistant","message":{"id":"resume-message","content":[{"type":"tool_use","id":"send","name":"SendMessage","input":{"to":"child","message":"Continue the audit"}}]}}),
                json!({"type":"user","session_id":"session","message":{"content":[{"type":"tool_result","tool_use_id":"send","content":[{"type":"text","text":"{\"success\":true,\"resumedAgentId\":\"child\"}"}]}]},"tool_use_result":{"success":true,"resumedAgentId":"child"}}),
            ] {
                events.extend(
                    mapper
                        .on_wire_message(msg, restart.then_some(root.as_path()))
                        .await,
                );
            }
            events.extend(mapper.on_wire_message(json!({"type":"system","subtype":"task_notification","session_id":"session","task_id":"child","tool_use_id":"send","status":"completed","summary":"Audit complete"}), restart.then_some(root.as_path())).await);

            assert!(events.iter().any(|event| matches!(event,
                    AgentEvent::ItemUpdated(ThreadItem { id, content: ItemContent::Subagent {status: ItemStatus::Completed, ..}, .. }) if id == "spawn"
                )), "completion must settle the original mirror (restart={restart})");
            assert!(events.iter().any(|event| matches!(event,
                    AgentEvent::ItemUpdated(ThreadItem { id, content: ItemContent::Subagent {status: ItemStatus::InProgress, ..}, .. }) if id == "spawn"
                )), "resuming must reopen the original mirror (restart={restart})");
            assert!(events.iter().any(|event| matches!(event,
                    AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::ToolCall {status: ItemStatus::Completed, ..}, .. }) if id == "send"
                )), "SendMessage must retain its own tool result");
        }
        let _ = std::fs::remove_dir_all(root);
    });
}

/// A background subagent's own background Agent call is that subagent's.
/// Claude Code 2.1.319 reports the nested task on the lead's stream with
/// the launching subagent's task as `parent_task_id`, before the
/// subagent's transcript tail shows the call; the lead's Agents view
/// lists only Subagent items without a parent, so the nested one must
/// never be one, whether or not its metadata file is readable yet.
#[test]
fn a_subagents_background_agent_stays_with_that_subagent() {
    smol::block_on(async {
        let root = std::env::temp_dir().join(format!("tcode-nested-bg-{}", uuid::Uuid::new_v4()));
        let dir = root.join("projects/project/session/subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("agent-a-child.meta.json"),
                r#"{"agentType":"general-purpose","description":"Check the UI","toolUseId":"toolu_child","spawnDepth":1,"requestShape":"background","requestNonInteractive":true}"#
            ).unwrap();
        std::fs::write(dir.join("agent-a-nested.meta.json"),
                r#"{"agentType":"general-purpose","description":"Visual check: headless screenshots","toolUseId":"toolu_nested","parentAgentId":"a-child","spawnDepth":2,"requestShape":"background","requestNonInteractive":true}"#
            ).unwrap();
        for metadata in [Some(root.as_path()), None] {
            let mut mapper = Mapper::new();
            let mut events = Vec::new();
            for line in [
                r#"{"type":"assistant","session_id":"session","message":{"id":"msg-spawn","content":[{"type":"tool_use","id":"toolu_child","name":"Agent","input":{"description":"Check the UI","prompt":"Check the UI.","subagent_type":"general-purpose","run_in_background":true}}]}}"#,
                r#"{"type":"system","subtype":"task_started","session_id":"session","task_id":"a-child","tool_use_id":"toolu_child","description":"Check the UI","subagent_type":"general-purpose","is_backgrounded":true,"spawn_depth":1,"task_type":"local_agent","prompt":"Check the UI."}"#,
                r#"{"type":"user","session_id":"session","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_child","content":"Async agent launched successfully.\nagentId: a-child"}]},"tool_use_result":{"isAsync":true,"status":"async_launched","agentId":"a-child"}}"#,
                r#"{"type":"system","subtype":"task_started","session_id":"session","task_id":"a-nested","tool_use_id":"toolu_nested","description":"Visual check: headless screenshots","subagent_type":"general-purpose","is_backgrounded":true,"spawn_depth":2,"parent_task_id":"a-child","task_type":"local_agent","prompt":"Take headless screenshots."}"#,
                r#"{"type":"system","subtype":"background_tasks_changed","session_id":"session","tasks":[{"task_id":"a-child","task_type":"local_agent","subagent_type":"general-purpose","description":"Check the UI"},{"task_id":"a-nested","task_type":"local_agent","subagent_type":"general-purpose","description":"Visual check: headless screenshots","parent_task_id":"a-child"}]}"#,
            ] {
                let msg = serde_json::from_str(line).unwrap();
                events.extend(mapper.on_wire_message(msg, metadata).await);
            }
            // The child's transcript shows the call and its launch only now.
            let mut child = crate::subagent_tail::TranscriptMapper::new("toolu_child");
            let mut transcript = Vec::new();
            for line in [
                r#"{"type":"assistant","message":{"id":"msg-child","content":[{"type":"tool_use","id":"toolu_nested","name":"Agent","input":{"description":"Visual check: headless screenshots","prompt":"Take headless screenshots.","subagent_type":"general-purpose","run_in_background":true}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_nested","content":[{"type":"text","text":"Async agent launched successfully.\nagentId: a-nested"}]}]}}"#,
            ] {
                transcript.extend(child.map_value(&serde_json::from_str(line).unwrap()));
            }
            events.extend(mapper.on_tail_notice(SubagentTailNotice {
                parent_id: "toolu_child".into(),
                events: transcript,
                model: None,
                stopped: false,
            }));
            events.extend(mapper.on_wire_message(serde_json::from_str(
                    r#"{"type":"system","subtype":"task_notification","session_id":"session","task_id":"a-nested","tool_use_id":"toolu_nested","status":"completed","output_file":"","summary":"Screenshots taken"}"#,
                ).unwrap(), metadata).await);
            events.extend(mapper.on_tail_notice(SubagentTailNotice {
                parent_id: "toolu_nested".into(),
                events: Vec::new(),
                model: None,
                stopped: true,
            }));

            let snapshots: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::ItemStarted(item)
                    | AgentEvent::ItemUpdated(item)
                    | AgentEvent::ItemCompleted(item)
                        if matches!(item.content, ItemContent::Subagent { .. }) =>
                    {
                        Some(item)
                    }
                    _ => None,
                })
                .collect();
            let leads: Vec<_> = snapshots
                .iter()
                .filter(|item| item.parent_item_id.is_none())
                .map(|item| item.id.as_str())
                .collect();
            assert!(
                leads.iter().all(|id| *id == "toolu_child"),
                "only the lead's own call is the lead's agent (metadata={metadata:?}): {snapshots:?}"
            );
            let nested: Vec<_> = snapshots
                .iter()
                .filter(|item| item.id == "toolu_nested")
                .collect();
            assert!(
                matches!(
                    nested.first(),
                    Some(ThreadItem { parent_item_id: Some(parent), content: ItemContent::Subagent { description, status: ItemStatus::InProgress, .. }, .. })
                        if parent == "toolu_child" && description == "Visual check: headless screenshots"
                ),
                "the nested agent is the subagent's from its task on (metadata={metadata:?}): {nested:?}"
            );
            assert!(
                matches!(
                    nested.last(),
                    Some(ThreadItem { content: ItemContent::Subagent { status: ItemStatus::Completed, summary: Some(summary), .. }, .. })
                        if summary == "Screenshots taken"
                ),
                "{nested:?}"
            );
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    AgentEvent::ItemBackgrounded { item_id, parent_item_id: Some(parent) }
                        if item_id == "toolu_nested" && parent == "toolu_child"
                )),
                "the launch returns the subagent's call: {events:?}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    });
}

/// A nested Agent that runs in the foreground settles with the child's
/// tool_result; a coordination tool is not a spawn.
#[test]
fn nested_foreground_subagent_completes_on_the_childs_tool_result() {
    let mut mapper = Mapper::new();
    let mut events = feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg-spawn","content":[{"type":"tool_use","id":"toolu_child","name":"Agent","input":{"description":"Audit","prompt":"Audit routing.","subagent_type":"Explore"}}]}}"#,
    );
    events.extend(feed(
            &mut mapper,
            r#"{"type":"assistant","parent_tool_use_id":"toolu_child","message":{"id":"msg-child-1","content":[{"type":"tool_use","id":"toolu_list","name":"ListAgents","input":{}},{"type":"tool_use","id":"toolu_nested","name":"Agent","input":{"description":"Check tests","prompt":"Check the tests.","subagent_type":"Explore"}}]}}"#,
        ));
    events.extend(feed(
            &mut mapper,
            r#"{"type":"user","parent_tool_use_id":"toolu_child","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_list","content":"none"},{"type":"tool_result","tool_use_id":"toolu_nested","content":[{"type":"text","text":"Tests   are\ngreen."}]}]}}"#,
        ));
    assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemStarted(ThreadItem { id, parent_item_id: Some(parent), content: ItemContent::ToolCall { name, .. }, .. })
                if id == "toolu_child:toolu_list" && parent == "toolu_child" && name == "ListAgents"
        )));
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::ItemCompleted(ThreadItem { id, parent_item_id: Some(parent), content: ItemContent::Subagent { agent_type, status: ItemStatus::Completed, summary: Some(summary), .. }, .. }))
                if id == "toolu_nested" && parent == "toolu_child" && agent_type == "Explore" && summary == "Tests   are\ngreen."
        ),
        "{events:?}"
    );
}

/// Claude Code 2.1.322 builds a `task_notification` with
/// `output_file: outputFile ?? ""`: an empty string is no file, and a
/// tail told to prefer it would read nothing and stop looking for the
/// subagent's transcript.
#[test]
fn a_notification_without_an_output_file_names_no_path_to_tail() {
    let mut mapper = Mapper::new();
    feed(
        &mut mapper,
        r#"{"type":"system","subtype":"task_notification","session_id":"session","task_id":"a-child","tool_use_id":"toolu_child","status":"completed","output_file":"","summary":"Done"}"#,
    );
    let requests = mapper.take_tail_requests();
    assert!(
        !requests
            .iter()
            .any(|request| matches!(request, TailRequest::PreferPath { .. }))
    );
    assert!(matches!(
        &requests[..],
        [TailRequest::Stop { parent_id }] if parent_id == "toolu_child"
    ));
}

/// A recorded stdout trace driven as the actor drives it: every line, then
/// the acknowledgement of each tail the trace stopped.
fn replay_subagent_trace<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<AgentEvent> {
    let mut mapper = Mapper::new();
    let mut events = Vec::new();
    for line in lines {
        events.extend(feed(&mut mapper, line));
    }
    for request in mapper.take_tail_requests() {
        if let TailRequest::Stop { parent_id } = request {
            events.extend(mapper.on_tail_notice(SubagentTailNotice {
                parent_id,
                events: Vec::new(),
                model: None,
                stopped: true,
            }));
        }
    }
    events
}

/// The summary a thread shows for `id` once it has settled: a snapshot
/// without one leaves the last one standing.
#[track_caller]
fn settled_summary(events: &[AgentEvent], id: &str) -> Option<String> {
    let mut shown = None;
    let mut settled = false;
    for event in events {
        if let AgentEvent::ItemUpdated(item) | AgentEvent::ItemCompleted(item) = event
            && item.id == id
            && let ItemContent::Subagent {
                status, summary, ..
            } = &item.content
        {
            shown = summary.clone().or(shown);
            settled |= *status == ItemStatus::Completed;
        }
    }
    assert!(settled, "{id} never settled: {events:?}");
    shown
}

#[track_caller]
fn assert_no_handback_note(events: &[AgentEvent]) {
    for event in events {
        if let AgentEvent::ItemUpdated(item) | AgentEvent::ItemCompleted(item) = event
            && let ItemContent::Subagent {
                summary: Some(summary),
                ..
            } = &item.content
        {
            assert!(!is_handback_note(summary), "{event:?}");
        }
    }
}

#[test]
fn a_foreground_subagent_that_hands_back_shows_its_report_whole() {
    let trace = include_str!("../../tests/fixtures/claude/subagent_handback_trace.jsonl");
    let events = replay_subagent_trace(trace.lines());
    assert_eq!(
        settled_summary(&events, "toolu_010000000000000000000007").as_deref(),
        Some("PONG-FG-REPORT\nsecond-line")
    );
    assert_no_handback_note(&events);
}

#[test]
fn a_background_subagent_that_hands_back_shows_its_report() {
    let trace =
        include_str!("../../tests/fixtures/claude/subagent_handback_background_trace.jsonl");
    let events = replay_subagent_trace(trace.lines());
    assert_eq!(
        settled_summary(&events, "toolu_010000000000000000000002").as_deref(),
        Some("PONG-BG-REPORT")
    );
    assert_no_handback_note(&events);
}

/// The tool_result of a nested Agent carries only the note, so the
/// nested agent's report is the one its task_notification brought.
#[test]
fn a_nested_subagent_that_hands_back_shows_its_report() {
    let trace = include_str!("../../tests/fixtures/claude/subagent_handback_nested_trace.jsonl");
    let events = replay_subagent_trace(trace.lines());
    assert_eq!(
        settled_summary(&events, "toolu_010000000000000000000005").as_deref(),
        Some("PONG-INNER-REPORT")
    );
    assert_eq!(
        settled_summary(&events, "toolu_010000000000000000000004").as_deref(),
        Some("PONG-OUTER-REPORT")
    );
    assert_no_handback_note(&events);
}

#[test]
fn a_subagent_that_delivered_no_report_has_no_summary() {
    let trace = include_str!("../../tests/fixtures/claude/subagent_withheld_trace.jsonl");
    let events = replay_subagent_trace(trace.lines());
    assert_eq!(
        settled_summary(&events, "toolu_010000000000000000000013"),
        None
    );
    assert_no_handback_note(&events);
}

#[test]
fn a_nested_subagent_that_delivered_no_report_has_no_summary() {
    let trace = include_str!("../../tests/fixtures/claude/subagent_withheld_nested_trace.jsonl");
    let events = replay_subagent_trace(trace.lines());
    assert_eq!(
        settled_summary(&events, "toolu_010000000000000000000011"),
        None
    );
    assert_eq!(
        settled_summary(&events, "toolu_010000000000000000000010").as_deref(),
        Some("OUTER-REPORT")
    );
    assert_no_handback_note(&events);
}

/// Outside auto mode a subagent has no `SubagentHandback`: it ends with
/// text, and the task_notification's `summary` is that text.
#[test]
fn a_subagent_that_ends_with_text_keeps_it() {
    let trace = include_str!("../../tests/fixtures/claude/subagent_text_end_trace.jsonl");
    let events = replay_subagent_trace(trace.lines());
    assert_eq!(
        settled_summary(&events, "toolu_010000000000000000000009").as_deref(),
        Some("PONG-FG-REPORT")
    );
}

/// No recording: auto mode has to block the report. The fields are those
/// of the CLI's own schema, which has a client show `warning` above `text`.
#[test]
fn a_flagged_report_keeps_its_warning_above_it() {
    let trace = include_str!("../../tests/fixtures/claude/subagent_handback_trace.jsonl")
        .replace("\"handback\":\"send\"", "\"handback\":\"flagged\"")
        .replace(
            "{\"text\":\"PONG-FG-REPORT",
            "{\"warning\":\"SECURITY WARNING\",\"text\":\"PONG-FG-REPORT",
        );
    let events = replay_subagent_trace(trace.lines());
    assert_eq!(
        settled_summary(&events, "toolu_010000000000000000000007").as_deref(),
        Some("SECURITY WARNING\n\nPONG-FG-REPORT\nsecond-line")
    );
}

/// The tail keeps the transcript discovery found; the notification's
/// `output_file` only seeds a reader when discovery found nothing.
#[test]
fn subagent_tail_prefer_path_does_not_replay_a_discovered_transcript() {
    let root = std::env::temp_dir().join(format!(
        "tcode-subagent-tail-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let subagents = root.join("projects/session-tail/subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    let transcript = "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n";
    std::fs::write(
        subagents.join("agent-x.meta.json"),
        r#"{"toolUseId":"toolu_tail"}"#,
    )
    .unwrap();
    std::fs::write(subagents.join("agent-x.jsonl"), transcript).unwrap();
    let output_file = root.join("task.output");
    std::fs::write(&output_file, transcript).unwrap();

    let run = |claude_dir: Option<PathBuf>| {
        let (control_tx, control_rx) = smol::channel::unbounded();
        let (notice_tx, notice_rx) = smol::channel::unbounded();
        smol::spawn(run_subagent_tail(
            "toolu_tail".into(),
            "task-tail".into(),
            "session-tail".into(),
            claude_dir,
            control_rx,
            notice_tx,
        ))
        .detach();
        (control_tx, notice_rx)
    };
    smol::block_on(async {
        let (control, notices) = run(Some(root.clone()));
        let first = notices.recv().await.unwrap();
        assert_eq!(first.events.len(), 2);
        assert!(!first.stopped);
        control
            .send(TailControl::PreferPath(output_file.clone()))
            .await
            .unwrap();
        control.send(TailControl::Stop).await.unwrap();
        let mut after = Vec::new();
        loop {
            let notice = notices.recv().await.unwrap();
            after.extend(notice.events);
            if notice.stopped {
                break;
            }
        }
        assert!(
            after.is_empty(),
            "preferred path must not be re-read: {after:?}"
        );

        let (control, notices) = run(None);
        control
            .send(TailControl::PreferPath(output_file.clone()))
            .await
            .unwrap();
        control.send(TailControl::Stop).await.unwrap();
        let mut fallback = Vec::new();
        loop {
            let notice = notices.recv().await.unwrap();
            fallback.extend(notice.events);
            if notice.stopped {
                break;
            }
        }
        assert_eq!(
            fallback.len(),
            2,
            "undiscovered tail reads the notified file"
        );
    });
    let _ = std::fs::remove_dir_all(root);
}

/// Claude Code 2.1.322 emits a terminal `task_updated` and then the
/// `task_notification` naming `output_file` (its task-state update emits
/// one after the other), and the actor hands the tail each line's
/// requests before reading the next. A tail that discovery could not
/// point at a transcript still reads the notified file before it stops.
#[test]
fn an_undiscovered_subagent_tail_reads_the_notified_output_file() {
    const SPAWN: &str = "toolu_010000000000000000000002";
    const RECORDED_OUTPUT: &str = "/tmp/claude-1000/-home-user-project/00000000-0000-4000-8000-000000000002/tasks/a0000000000000001.output";
    let trace =
        include_str!("../../tests/fixtures/claude/subagent_handback_background_trace.jsonl");
    let root = std::env::temp_dir().join(format!("tcode-notified-output-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let output_file = root.join("a0000000000000001.output");
    // The transcript holds the child's own records, which stdout also
    // forwards with `parent_tool_use_id`.
    let child = format!("\"parent_tool_use_id\":\"{SPAWN}\"");
    let transcript: String = trace
        .lines()
        .filter(|line| line.contains(&child))
        .map(|line| format!("{line}\n"))
        .collect();
    std::fs::write(&output_file, transcript).unwrap();
    // The path goes into JSON text, where a Windows separator is an escape.
    let escaped = serde_json::to_string(output_file.to_str().unwrap()).unwrap();
    let trace = trace.replace(RECORDED_OUTPUT, escaped.trim_matches('"'));

    smol::block_on(async {
        let mut mapper = Mapper::new();
        let mut tailers = HashMap::new();
        let (tail_tx, tail_rx) = smol::channel::unbounded();
        let mut tailed = Vec::new();
        for line in trace.lines() {
            feed(&mut mapper, line);
            let requests = mapper.take_tail_requests();
            let stops_a_live_tail = tailers.contains_key(SPAWN)
                && requests
                    .iter()
                    .any(|request| matches!(request, TailRequest::Stop { .. }));
            process_tail_requests(requests, &mut tailers, None, &tail_tx).await;
            if !stops_a_live_tail {
                continue;
            }
            loop {
                let notice = tail_rx.recv().await.unwrap();
                tailed.extend(notice.events.iter().cloned());
                let stopped = notice.stopped;
                mapper.on_tail_notice(notice);
                if stopped {
                    tailers.remove(SPAWN);
                    break;
                }
            }
        }
        assert!(tailers.is_empty(), "the tail was never stopped");
        assert!(
            tailed.iter().any(|event| matches!(
                event,
                AgentEvent::ItemCompleted(ThreadItem { id, .. })
                    if *id == format!("{SPAWN}:toolu_010000000000000000000003")
            )),
            "the tail never read {output_file:?}: {tailed:?}"
        );
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn turn_acceptance_requires_a_flushed_write_and_steer_failures_remain_fatal() {
    smol::block_on(async {
        use std::io;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum FailurePoint {
            Write,
            Flush,
        }

        #[derive(Default)]
        struct DeterministicWriter {
            failure: Option<FailurePoint>,
            bytes: Vec<u8>,
        }

        impl AsyncWrite for DeterministicWriter {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                bytes: &[u8],
            ) -> Poll<io::Result<usize>> {
                if self.failure == Some(FailurePoint::Write) {
                    Poll::Ready(Err(io::Error::other("deterministic write failure")))
                } else {
                    self.bytes.extend_from_slice(bytes);
                    Poll::Ready(Ok(bytes.len()))
                }
            }

            fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                if self.failure == Some(FailurePoint::Flush) {
                    Poll::Ready(Err(io::Error::other("deterministic flush failure")))
                } else {
                    Poll::Ready(Ok(()))
                }
            }

            fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        let message = user_message("deliver", &[]);
        let (event_tx, event_rx) = smol::channel::unbounded();
        let mut writer = DeterministicWriter::default();
        write_turn_message(&mut writer, &message, 42, &event_tx)
            .await
            .unwrap();
        assert!(matches!(
            event_rx.recv().await.unwrap(),
            AgentEvent::TurnAccepted { delivery_id: 42 }
        ));
        assert_eq!(writer.bytes.last(), Some(&b'\n'));

        let (event_tx, event_rx) = smol::channel::unbounded();
        let mut writer = DeterministicWriter {
            failure: Some(FailurePoint::Flush),
            ..Default::default()
        };
        assert!(
            write_turn_message(&mut writer, &message, 43, &event_tx)
                .await
                .is_err()
        );
        assert!(event_rx.try_recv().is_err(), "failed flush must not ack");

        let (event_tx, event_rx) = smol::channel::unbounded();
        let mut writer = DeterministicWriter::default();
        let mut mapper = Mapper::new();
        let message = user_message("redirect", &[]);
        write_steering_message(
            &mut writer,
            &message,
            "steer-ok".into(),
            &mut mapper,
            &event_tx,
        )
        .await;
        assert!(event_rx.try_recv().is_err());
        assert_eq!(writer.bytes.last(), Some(&b'\n'));

        let (event_tx, event_rx) = smol::channel::unbounded();
        let mut mapper = Mapper::new();
        let mut writer = DeterministicWriter {
            failure: Some(FailurePoint::Write),
            ..Default::default()
        };
        write_steering_message(
            &mut writer,
            &message,
            "steer-write-failed".into(),
            &mut mapper,
            &event_tx,
        )
        .await;
        assert!(matches!(
            event_rx.recv().await.unwrap(),
            AgentEvent::Error { fatal: true, .. }
        ));
        assert!(
            event_rx.try_recv().is_err(),
            "stdin write failures must not accept a steer"
        );

        let (event_tx, event_rx) = smol::channel::unbounded();
        let mut mapper = Mapper::new();
        let mut writer = DeterministicWriter {
            failure: Some(FailurePoint::Flush),
            ..Default::default()
        };
        write_steering_message(
            &mut writer,
            &message,
            "steer-flush-failed".into(),
            &mut mapper,
            &event_tx,
        )
        .await;
        assert!(matches!(
            event_rx.recv().await.unwrap(),
            AgentEvent::Error { fatal: true, .. }
        ));
        assert!(
            event_rx.try_recv().is_err(),
            "stdin flush failures must not accept a steer"
        );
    });
}

fn accepted_request_ids(events: &[AgentEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SteerAccepted { request_id } => Some(request_id.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn steer_acceptance_uses_request_boundaries_with_a_legacy_assistant_fallback() {
    let requesting = r#"{"type":"system","subtype":"status","status":"requesting","uuid":"checkpoint","session_id":"session"}"#;
    let assistant = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","id":"msg","content":[{"type":"text","text":"response"}]},"session_id":"session"}"#;
    for current_cli in [false, true] {
        let mut mapper = Mapper::new();
        if current_cli {
            assert!(feed(&mut mapper, requesting).is_empty());
        }
        mapper
            .pending_steers
            .extend(["first".into(), "second".into()]);
        let result = feed(
            &mut mapper,
            r#"{"type":"result","subtype":"success","is_error":false,"session_id":"session"}"#,
        );
        assert!(accepted_request_ids(&result).is_empty());
        let events = feed(&mut mapper, assistant);
        if current_cli {
            assert!(accepted_request_ids(&events).is_empty());
            assert_eq!(
                accepted_request_ids(&feed(&mut mapper, requesting)),
                ["first", "second"]
            );
        } else {
            assert_eq!(accepted_request_ids(&events), ["first", "second"]);
        }
        assert!(accepted_request_ids(&feed(&mut mapper, requesting)).is_empty());
        assert!(accepted_request_ids(&feed(&mut mapper, assistant)).is_empty());
    }
}

#[test]
fn refusal_stop_reason_warns_once_across_delta_and_assistant() {
    let mut mapper = Mapper::new();
    let delta = feed(
        &mut mapper,
        r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"refusal"}}}"#,
    );
    let assistant = feed(
        &mut mapper,
        r#"{"type":"assistant","message":{"id":"msg-1","stop_reason":"refusal","content":[]}}"#,
    );
    assert!(matches!(delta.as_slice(), [AgentEvent::Warning { .. }]));
    assert!(assistant.is_empty());
}
