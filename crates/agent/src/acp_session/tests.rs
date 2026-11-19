use super::*;
use serde_json::json;

fn state() -> State {
    State::new(PathBuf::from("/tmp/tcode-acp-test"))
}

fn update(json: Value) -> acp::SessionUpdate {
    serde_json::from_value(json).expect("valid session/update payload")
}

/// An agent that never answers `initialize` (cline 3.0.39) holds its
/// session's start until the session is stopped. The agent here is a
/// process that reads its stdin and says nothing; it tells the test when
/// `initialize` has reached it, so the stop lands on a start that is
/// waiting for the answer.
#[test]
fn a_start_its_agent_never_answers_ends_when_the_session_stops() {
    use std::io::BufRead as _;

    const ROLE: &str = "TCODE_TEST_SILENT_AGENT";
    const NAME: &str =
        "acp_session::tests::a_start_its_agent_never_answers_ends_when_the_session_stops";
    if let Ok(port) = std::env::var(ROLE) {
        let mut stdin = std::io::stdin().lock();
        let mut initialize = String::new();
        stdin.read_line(&mut initialize).unwrap();
        std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
        let _ = std::io::copy(&mut stdin, &mut std::io::sink());
        return;
    }
    let asked = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = asked.local_addr().unwrap().port().to_string();
    let processes = crate::SessionProcesses::default();
    let opts = SessionOptions {
        cwd: std::env::temp_dir(),
        model: None,
        resume: None,
        fork: None,
        binary_path: None,
        option_selections: Vec::new(),
        mcp_servers: Vec::new(),
        launch_env: crate::LaunchEnv::default(),
        extra_args: Vec::new(),
        acp: Some(crate::AcpAgent {
            id: "silent".into(),
            name: "Silent".into(),
            launch: crate::AcpLaunch::Binary {
                command: std::env::current_exe().unwrap(),
                args: vec!["--exact".into(), NAME.into(), "--quiet".into()],
                env: vec![(ROLE.into(), port)],
            },
            http_mcp: false,
        }),
        instructions: None,
        processes: processes.clone(),
        clock: Default::default(),
        worktrees: Vec::new(),
    };
    let start = smol::spawn(crate::start_session(ProviderKind::Acp, opts));
    asked.accept().unwrap();
    processes.stop();
    assert!(smol::block_on(start).is_err());
}

#[test]
fn prompt_acceptance_follows_the_complete_stdio_write() {
    smol::block_on(async {
        use smol::prelude::*;

        let pending = Arc::new(Mutex::new(VecDeque::from([41])));
        let (events, received) = smol::channel::unbounded();
        let inner = smol::io::Cursor::new(Vec::new());
        let mut writer = ObservedWriter::new(inner, pending.clone(), events);

        writer
            .write_all(
                br#"{"jsonrpc":"2.0","method":"initialize"}
"#,
            )
            .await
            .unwrap();
        writer
            .write_all(br#"{"jsonrpc":"2.0","method":"session/prompt"}"#)
            .await
            .unwrap();
        assert!(
            received.try_recv().is_err(),
            "an incomplete line must not ack"
        );

        writer.write_all(b"\n").await.unwrap();
        assert!(matches!(
            received.recv().await.unwrap(),
            AgentEvent::TurnAccepted { delivery_id: 41 }
        ));
    });
}

/// Tcode's MCP servers are loopback HTTP endpoints: they are handed to an
/// agent that advertises `mcpCapabilities.http`, or whose user says it takes
/// them anyway, and to no other.
#[test]
fn mcp_servers_go_to_an_agent_that_declares_http_or_is_set_to_take_it() {
    let registration = McpRegistration {
        name: McpRegistration::SERVER_NAME_PREVIEW.into(),
        url: "http://127.0.0.1:5321/mcp".into(),
        bearer_token: "tok".into(),
    };
    let control = McpRegistration {
        name: McpRegistration::SERVER_NAME_CONTROL.into(),
        url: "http://127.0.0.1:5322/mcp".into(),
        bearer_token: "other".into(),
    };
    let offered = json!([
        {"type": "http", "name": "tcode_preview", "url": "http://127.0.0.1:5321/mcp",
            "headers": [{"name": "Authorization", "value": "Bearer tok"}]},
        {"type": "http", "name": "tcode_control", "url": "http://127.0.0.1:5322/mcp",
            "headers": [{"name": "Authorization", "value": "Bearer other"}]},
    ]);
    let mut caps = acp::AgentCapabilities::default();
    assert!(
        mcp_servers(&[&registration, &control], &caps, false).is_empty(),
        "an agent without mcpCapabilities.http must not be sent the HTTP servers"
    );
    assert_eq!(
        serde_json::to_value(mcp_servers(&[&registration, &control], &caps, true)).unwrap(),
        offered,
        "the user's setting offers them without the declaration"
    );

    caps.mcp_capabilities.http = true;
    assert_eq!(
        serde_json::to_value(mcp_servers(&[&registration, &control], &caps, false)).unwrap(),
        offered
    );
}

#[test]
fn streamed_text_keeps_block_identity_across_reasoning_and_ignores_user_echoes() {
    let mut state = state();
    let mut previous = None;
    for (kind, wire_kind, chunks, expected) in [
        (
            DeltaKind::AssistantText,
            "agent_message_chunk",
            ["Hel", "lo"],
            "Hello",
        ),
        (
            DeltaKind::ReasoningText,
            "agent_thought_chunk",
            ["Think", "ing"],
            "Thinking",
        ),
        (
            DeltaKind::AssistantText,
            "agent_message_chunk",
            ["An", "swer"],
            "Answer",
        ),
    ] {
        let mut id = None;
        for chunk in chunks {
            let mut events = state
                .apply_update(update(json!({
                    "sessionUpdate": wire_kind, "content": {"type":"text", "text":chunk}
                })))
                .into_iter();
            if let Some((previous_id, previous_kind, previous_text)) = previous.take() {
                let Some(AgentEvent::ItemCompleted(item)) = events.next() else {
                    panic!("changing text kind must complete the previous block");
                };
                assert_eq!(item.id, previous_id);
                match (previous_kind, item.content) {
                    (DeltaKind::AssistantText, ItemContent::AssistantMessage { text, .. })
                    | (DeltaKind::ReasoningText, ItemContent::Reasoning { text }) => {
                        assert_eq!(text, previous_text)
                    }
                    other => panic!("wrong completed block: {other:?}"),
                }
            }
            let Some(AgentEvent::Delta {
                item_id,
                kind: actual_kind,
                text,
            }) = events.next()
            else {
                panic!("text chunk must stream a delta");
            };
            assert_eq!(actual_kind, kind);
            assert_eq!(text, chunk);
            if let Some(id) = &id {
                assert_eq!(&item_id, id);
            }
            id = Some(item_id);
            assert!(events.next().is_none());
            assert!(state.apply_update(update(json!({
                    "sessionUpdate":"user_message_chunk", "content":{"type":"text", "text":"prompt echo"}
                }))).is_empty());
        }
        previous = Some((id.unwrap(), kind, expected));
    }
    let (id, _, expected) = previous.unwrap();
    assert!(matches!(state.flush_text().as_slice(),
            [AgentEvent::ItemCompleted(ThreadItem { id: actual_id, content: ItemContent::AssistantMessage { text, .. }, .. })]
                if actual_id == &id && text == expected));
    assert!(state.flush_text().is_empty());
}

#[test]
fn execute_tool_call_maps_to_a_command_execution() {
    let mut state = state();
    let events = state.apply_update(update(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "t1",
        "title": "Run tests",
        "kind": "execute",
        "status": "in_progress",
        "rawInput": { "command": "cargo test" }
    })));
    match &events[0] {
        AgentEvent::ItemStarted(item) => {
            assert_eq!(item.id, "t1");
            match &item.content {
                ItemContent::CommandExecution {
                    command, status, ..
                } => {
                    assert_eq!(command, "cargo test");
                    assert_eq!(*status, ItemStatus::InProgress);
                }
                other => panic!("expected CommandExecution, got {other:?}"),
            }
        }
        other => panic!("expected ItemStarted, got {other:?}"),
    }

    // A partial patch merges into the merged state and completes the item.
    let events = state.apply_update(update(json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "t1",
        "status": "completed",
        "content": [{ "type": "content", "content": { "type": "text", "text": "ok" } }],
        "rawOutput": { "exitCode": 0 }
    })));
    match &events[0] {
        AgentEvent::ItemCompleted(item) => match &item.content {
            ItemContent::CommandExecution {
                command,
                input: _,
                output,
                exit_code,
                status,
            } => {
                assert_eq!(command, "cargo test", "rawInput must survive the patch");
                assert_eq!(output, "ok");
                assert_eq!(*exit_code, Some(0));
                assert_eq!(*status, ItemStatus::Completed);
            }
            other => panic!("expected CommandExecution, got {other:?}"),
        },
        other => panic!("expected ItemCompleted, got {other:?}"),
    }
}

#[test]
fn file_tools_preserve_operation_kind_and_render_available_change_evidence() {
    for (kind, old, new, operation, diff) in [
        (
            "edit",
            Some("same\nbefore\nend\n"),
            "same\nafter\nend\n",
            FileChangeKind::Modify,
            "--- a/file.rs\n+++ b/file.rs\n@@ -1,3 +1,3 @@\n same\n-before\n+after\n end\n",
        ),
        (
            "edit",
            None,
            "new\n",
            FileChangeKind::Create,
            "--- /dev/null\n+++ b/file.rs\n@@ -0,0 +1,1 @@\n+new\n",
        ),
        (
            "edit",
            Some(""),
            "new\n",
            FileChangeKind::Create,
            "--- /dev/null\n+++ b/file.rs\n@@ -0,0 +1,1 @@\n+new\n",
        ),
        (
            "delete",
            Some("old\n"),
            "",
            FileChangeKind::Delete,
            "--- a/file.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-old\n",
        ),
        (
            "move",
            Some("old\n"),
            "new\n",
            FileChangeKind::Rename,
            "--- a/file.rs\n+++ b/file.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n",
        ),
        // The kind a call has when its agent names none (schema 1.18.0).
        (
            "other",
            Some("old\n"),
            "new\n",
            FileChangeKind::Modify,
            "--- a/file.rs\n+++ b/file.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n",
        ),
    ] {
        let events = state().apply_update(update(json!({
            "sessionUpdate":"tool_call", "toolCallId":"file", "title":"Change file",
            "kind":kind, "status":"completed",
            "content":[{"type":"diff","path":"file.rs","oldText":old,"newText":new}]
        })));
        let [
            AgentEvent::ItemStarted(ThreadItem {
                content:
                    ItemContent::FileChange {
                        changes, status, ..
                    },
                ..
            }),
        ] = events.as_slice()
        else {
            panic!("{kind}: expected file item, got {events:?}");
        };
        assert_eq!(*status, ItemStatus::Completed);
        assert_eq!(
            changes,
            &[FileChange {
                path: "file.rs".into(),
                kind: operation,
                diff: Some(diff.into())
            }]
        );
    }
    // An edit's diff arrives once it is applied; until then it is the
    // files its locations name.
    for (kind, operation) in [
        ("delete", FileChangeKind::Delete),
        ("move", FileChangeKind::Rename),
        ("edit", FileChangeKind::Modify),
    ] {
        let events = state().apply_update(update(json!({
            "sessionUpdate":"tool_call", "toolCallId":"file", "title":"Change file",
            "kind":kind, "status":"completed", "locations":[{"path":"one.rs"},{"path":"two.rs"}]
        })));
        let [
            AgentEvent::ItemStarted(ThreadItem {
                content: ItemContent::FileChange { changes, .. },
                ..
            }),
        ] = events.as_slice()
        else {
            panic!("{kind}: expected file locations, got {events:?}");
        };
        assert_eq!(
            changes,
            &[
                FileChange {
                    path: "one.rs".into(),
                    kind: operation,
                    diff: None
                },
                FileChange {
                    path: "two.rs".into(),
                    kind: operation,
                    diff: None
                },
            ]
        );
    }
    let events = state().apply_update(update(json!({
        "sessionUpdate":"tool_call", "toolCallId":"file", "title":"Opaque edit",
        "kind":"edit", "status":"completed", "rawInput":{"path":"file.rs"}, "rawOutput":"done"
    })));
    assert!(
        matches!(events.as_slice(), [AgentEvent::ItemStarted(ThreadItem {
            content:ItemContent::ToolCall {
            image_reads: _, name, input, output:Some(output), status:ItemStatus::Completed,
            .. }, ..
        })] if name == "Opaque edit" && input == &json!({"path":"file.rs"}) && output == "done")
    );
}

/// ACP carries a tool's images as `image` content blocks: they become
/// the item's images whatever the tool's kind, never text.
#[test]
fn image_content_blocks_become_image_reads() {
    for kind in ["read", "other"] {
        let mut mapper = state();
        mapper.apply_update(update(json!({"sessionUpdate":"tool_call","toolCallId":"shot","title":"Screenshot","kind":kind,"status":"in_progress"})));
        let events = mapper.apply_update(update(json!({"sessionUpdate":"tool_call_update","toolCallId":"shot","status":"completed","content":[
                {"type":"content","content":{"type":"text","text":"captured"}},
                {"type":"content","content":{"type":"image","mimeType":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEElEQVR4nGNgYPiPAw0pCQCpcD/BFMrqcwAAAABJRU5ErkJggg=="}}
            ]})));
        let [
            AgentEvent::ItemCompleted(ThreadItem {
                content:
                    ItemContent::ToolCall {
                        image_reads,
                        output,
                        ..
                    },
                ..
            }),
        ] = &events[..]
        else {
            panic!("tool completion: {events:?}")
        };
        assert_eq!(output.as_deref(), Some("captured"), "{kind}");
        assert_eq!(image_reads.len(), 1, "{kind}");
        assert_eq!(image_reads[0].media_type, "image/png");
    }
}

/// A `think` call is reasoning; every other kind without a card of its
/// own is a tool card carrying its raw input and text output.
#[test]
fn other_tool_kinds_map_to_tool_cards_and_think_to_reasoning() {
    let card = ItemContent::ToolCall {
        name: "Read file".into(),
        input: json!({ "path": "/repo/x.rs" }),
        output: Some("body".into()),
        status: ItemStatus::Completed,
        image_reads: Vec::new(),
        family: None,
    };
    for (kind, expected) in [
        ("read", card.clone()),
        ("other", card),
        (
            "think",
            ItemContent::Reasoning {
                text: "body".into(),
            },
        ),
    ] {
        let events = state().apply_update(update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Read file",
            "kind": kind,
            "status": "completed",
            "rawInput": { "path": "/repo/x.rs" },
            "content": [{ "type": "content", "content": { "type": "text", "text": "body" } }]
        })));
        match events.as_slice() {
            [AgentEvent::ItemStarted(item)] => assert_eq!(item.content, expected, "{kind}"),
            other => panic!("{kind}: expected ItemStarted, got {other:?}"),
        }
    }
}

/// A `read` call is a read of the file its location names, from the
/// line it names (`ToolKind::Read`, `ToolCallLocation {path, line}` of
/// agent-client-protocol-schema 1.18.0).
#[test]
fn a_read_is_the_file_and_line_it_locates() {
    let events = state().apply_update(update(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "t1",
        "title": "Read file",
        "kind": "read",
        "status": "completed",
        "locations": [{ "path": "/repo/x.rs", "line": 12 }],
        "content": [{ "type": "content", "content": { "type": "text", "text": "body" } }]
    })));
    let [AgentEvent::ItemStarted(item)] = events.as_slice() else {
        panic!("{events:?}");
    };
    let ItemContent::ToolCall { family, .. } = &item.content else {
        panic!("{item:?}");
    };
    assert_eq!(
        family.as_deref(),
        Some(&ToolFamily::Read {
            path: "/repo/x.rs".into(),
            first_line: Some(12),
            note: None,
        })
    );
}

#[test]
fn tool_updates_recover_missing_starts_and_preserve_status_and_input() {
    for (wire, expected) in [
        ("pending", ItemStatus::InProgress),
        ("in_progress", ItemStatus::InProgress),
        ("completed", ItemStatus::Completed),
        ("failed", ItemStatus::Failed),
    ] {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate":"tool_call_update", "toolCallId":"late", "title":"Late tool",
            "kind":"other", "status":"pending", "rawInput":{"path":"a.rs"}
        })));
        assert!(matches!(events.as_slice(), [AgentEvent::ItemStarted(_)]));
        let events = state.apply_update(update(json!({
            "sessionUpdate":"tool_call_update", "toolCallId":"late", "status":wire
        })));
        let item = match events.as_slice() {
            [AgentEvent::ItemUpdated(item)] if expected == ItemStatus::InProgress => item,
            [AgentEvent::ItemCompleted(item)] if expected != ItemStatus::InProgress => item,
            other => panic!("{wire}: incorrect lifecycle event {other:?}"),
        };
        assert!(
            matches!(&item.content, ItemContent::ToolCall { name, input, status, .. }
                if name == "Late tool" && input == &json!({"path":"a.rs"}) && *status == expected)
        );
    }
}

#[test]
fn plan_update_preserves_turn_and_steps() {
    let mut state = state();
    state.turn = Some("turn-1".into());
    let events = state.apply_update(update(json!({
        "sessionUpdate": "plan",
        "entries": [
            { "content": "Read the code", "priority": "high", "status": "completed" },
            { "content": "Write the fix", "priority": "medium", "status": "in_progress" }
        ]
    })));
    match &events[0] {
        AgentEvent::PlanUpdated {
            turn_id,
            steps,
            explanation,
        } => {
            assert_eq!(turn_id.as_deref(), Some("turn-1"));
            assert!(explanation.is_none());
            assert_eq!(steps.len(), 2);
            assert_eq!(steps[0].step, "Read the code");
            assert_eq!(steps[0].status, PlanStepStatus::Completed);
            assert_eq!(steps[1].status, PlanStepStatus::InProgress);
        }
        other => panic!("expected PlanUpdated, got {other:?}"),
    }
}

#[test]
fn available_commands_become_provider_commands() {
    let mut state = state();
    let events = state.apply_update(update(json!({
        "sessionUpdate": "available_commands_update",
        "availableCommands": [
            { "name": "review", "description": "Review the diff", "input": null }
        ]
    })));
    match &events[0] {
        AgentEvent::ProviderCommands { commands } => {
            assert_eq!(commands[0].name, "review");
            assert_eq!(commands[0].kind, ProviderCommandKind::Command);
        }
        other => panic!("expected ProviderCommands, got {other:?}"),
    }
}

#[test]
fn usage_update_is_the_context_window() {
    let mut state = state();
    let events = state.apply_update(update(json!({
        "sessionUpdate": "usage_update",
        "used": 1200,
        "size": 200000
    })));
    match &events[0] {
        AgentEvent::TokenUsage(usage) => {
            assert_eq!(usage.used_tokens, Some(1200));
            assert_eq!(usage.context_window, Some(200_000));
        }
        other => panic!("expected TokenUsage, got {other:?}"),
    }
}

#[test]
fn modes_and_categorized_config_become_provider_options() {
    let mut state = state();
    let modes: acp::SessionModeState = serde_json::from_value(json!({
        "currentModeId": "build",
        "availableModes": [
            { "id": "build", "name": "Build" },
            { "id": "plan", "name": "Plan", "description": "Read-only" }
        ]
    }))
    .unwrap();
    let config: Vec<acp::SessionConfigOption> = serde_json::from_value(json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": "sonnet",
            "options": [{ "value": "sonnet", "name": "Sonnet" }]
        },
        {
            "id": "thought_level",
            "name": "Thinking",
            "category": "thought_level",
            "type": "select",
            "currentValue": "medium",
            "options": [
                { "value": "low", "name": "Low" },
                { "value": "medium", "name": "Medium" }
            ]
        },
        { "id": "web", "name": "Web search", "type": "boolean", "currentValue": true }
    ]))
    .unwrap();
    // As `Setup::adopt` takes what the session started with.
    state.ingest_modes(Some(&modes));
    state.options.ingest(None, Some(&config));

    let AgentEvent::ProviderOptions {
        descriptors,
        selections,
    } = state.provider_options()
    else {
        unreachable!();
    };
    let ids: Vec<&str> = descriptors.iter().map(descriptor_id).collect();
    assert_eq!(
        ids,
        vec!["acp:mode", "acp:model", "reasoningEffort", "acp:cfg:web"]
    );
    assert!(matches!(
        &descriptors[3],
        OptionDescriptor::Boolean {
            default_value: true,
            ..
        }
    ));
    let selections: Vec<_> = selections
        .iter()
        .map(|selection| (selection.id.as_str(), selection.value.clone()))
        .collect();
    assert_eq!(
        selections,
        [
            ("acp:mode", json!("build")),
            ("acp:model", json!("sonnet")),
            ("reasoningEffort", json!("medium")),
            ("acp:cfg:web", json!(true)),
        ]
    );

    // An agent-initiated mode switch re-publishes the options…
    let events = state.apply_update(update(json!({
        "sessionUpdate": "current_mode_update",
        "currentModeId": "plan"
    })));
    match &events[0] {
        AgentEvent::ProviderOptions { selections, .. } => {
            assert_eq!(selections[0].value, json!("plan"));
        }
        other => panic!("expected ProviderOptions, got {other:?}"),
    }

    // …and so does a config-option push.
    let events = state.apply_update(update(json!({
        "sessionUpdate": "config_option_update",
        "configOptions": [
            { "id": "web", "name": "Web search", "type": "boolean", "currentValue": false }
        ]
    })));
    match &events[0] {
        AgentEvent::ProviderOptions { selections, .. } => {
            let web = selections.iter().find(|s| s.id == "acp:cfg:web").unwrap();
            assert_eq!(web.value, json!(false));
        }
        other => panic!("expected ProviderOptions, got {other:?}"),
    }
}

/// Grok 1.0.46 asking to run a command, and Tcode's answer when the
/// person allowed it once, as recorded (`tests/fixtures/grok/README.md`).
#[test]
fn permission_request_carries_the_agents_own_options() {
    let recorded: Vec<Value> = include_str!("../../tests/fixtures/grok/resumed_turn.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["message"].take())
        .collect();
    let asked = recorded
        .iter()
        .position(|message| message["method"] == "session/request_permission")
        .unwrap();
    let answered = recorded[asked..]
        .iter()
        .find(|message| message["id"] == recorded[asked]["id"] && message.get("result").is_some())
        .unwrap();
    let request: acp::RequestPermissionRequest =
        serde_json::from_value(recorded[asked]["params"].clone()).unwrap();
    let approval = approval_request(
        "acp-approval-1".into(),
        Some("turn-1".into()),
        &request.tool_call,
        &request.options,
    );
    match &approval.kind {
        ApprovalKind::ExecCommand { command, .. } => {
            assert_eq!(command, "for i in 1 2 3; do echo line$i; sleep 1; done")
        }
        other => panic!("expected ExecCommand, got {other:?}"),
    }
    let options: Vec<_> = approval
        .options
        .iter()
        .map(|option| (option.id.as_str(), option.kind))
        .collect();
    assert_eq!(
        options,
        [
            ("always-allow", ApprovalOptionKind::AllowAlways),
            ("allow-once", ApprovalOptionKind::AllowOnce),
            ("reject-once", ApprovalOptionKind::RejectOnce),
            ("reject-always", ApprovalOptionKind::RejectAlways),
        ]
    );
    assert_eq!(approval.options[1].label, "Yes, proceed");
    let response = acp::RequestPermissionResponse::new(
        approval_outcome(
            &ApprovalDecision::Option("allow-once".into()),
            &approval.options,
        )
        .unwrap(),
    );
    assert_eq!(serde_json::to_value(&response).unwrap(), answered["result"]);
    assert!(matches!(
        approval_outcome(&ApprovalDecision::Cancel, &approval.options),
        Some(acp::RequestPermissionOutcome::Cancelled)
    ));
}

/// `fs/write_text_file` (ACP schema 1.18.0, `WriteTextFileRequest`) names
/// no tool call. gemini-cli 0.68.0 writes through it from inside an
/// `edit` call that locates the file, and completes the call with a
/// `diff` for a file it modified but only the text "Created …" for one it
/// created (`zedIntegration`'s `runTool`, `EditToolInvocation.execute`).
#[test]
fn a_file_written_for_a_call_is_that_calls_change_once() {
    let cwd = std::env::temp_dir().join(format!("tcode-acp-write-{}", uuid::Uuid::new_v4()));
    let path = cwd.join("notes/todo.md");
    let shown = path.to_string_lossy().into_owned();
    let call = |id: &str| {
        update(json!({
            "sessionUpdate":"tool_call", "toolCallId":id, "title":"Edit todo.md",
            "kind":"edit", "status":"in_progress", "locations":[{"path":path}]
        }))
    };
    let changes = |event: AgentEvent| match event {
        AgentEvent::ItemUpdated(item) | AgentEvent::ItemCompleted(item) => match item.content {
            ItemContent::FileChange { changes, .. } => changes,
            other => panic!("expected a file change, got {other:?}"),
        },
        other => panic!("expected the call's item, got {other:?}"),
    };
    smol::block_on(async {
        let (events, received) = smol::channel::unbounded();
        let client = Client {
            events,
            state: Arc::new(Mutex::new(State::new(cwd.clone()))),
        };
        let apply = |update| client.state.lock_recover().apply_update(update);
        let write = async |text: &str| {
            client
                .write_text_file(acp::WriteTextFileRequest::new("session-1", &path, text))
                .await
                .unwrap();
        };

        apply(call("call-1"));
        write("- buy milk\n").await;
        let created = [FileChange {
            path: shown.clone(),
            kind: FileChangeKind::Create,
            diff: Some(format!(
                "--- /dev/null\n+++ b/{shown}\n@@ -0,0 +1,1 @@\n+- buy milk\n"
            )),
        }];
        assert_eq!(changes(received.try_recv().unwrap()), created);
        let completed = apply(update(json!({
            "sessionUpdate":"tool_call_update", "toolCallId":"call-1", "status":"completed",
            "content":[{"type":"content","content":{"type":"text","text":"Created notes/todo.md"}}]
        })));
        assert_eq!(changes(completed.into_iter().next().unwrap()), created);

        apply(call("call-2"));
        write("- buy milk\n- call Ada\n").await;
        let modified =
            format!("--- a/{shown}\n+++ b/{shown}\n@@ -1,1 +1,2 @@\n - buy milk\n+- call Ada\n");
        assert_eq!(
            changes(received.try_recv().unwrap()),
            [FileChange {
                path: shown.clone(),
                kind: FileChangeKind::Modify,
                diff: Some(modified.clone()),
            }]
        );
        let completed = apply(update(json!({
            "sessionUpdate":"tool_call_update", "toolCallId":"call-2", "status":"completed",
            "content":[{"type":"diff","path":path,"oldText":"- buy milk\n","newText":"- buy milk\n- call Ada\n"}]
        })));
        assert_eq!(
            changes(completed.into_iter().next().unwrap()),
            [FileChange {
                path: shown.clone(),
                kind: FileChangeKind::Modify,
                diff: Some(modified),
            }],
            "the agent's own diff is the one account of the file"
        );
    });
    let _ = std::fs::remove_dir_all(&cwd);
}

/// A permission request ends without the user in the two ways ACP
/// (schema 1.18.0) has: Tcode answers `cancelled` on an interrupt, as
/// `session/cancel` requires, and the agent withdraws it with
/// `$/cancel_request`. Either way it is reported resolved.
#[test]
fn a_permission_request_ended_without_the_user_is_reported_resolved() {
    smol::block_on(async {
        let (events, received) = smol::channel::unbounded();
        let client = Client {
            events,
            state: Arc::new(Mutex::new(State::new(PathBuf::from("/workspace")))),
        };
        let request = || {
            acp::RequestPermissionRequest::new(
                "session-1",
                acp::ToolCallUpdate::new(
                    "call-1",
                    acp::ToolCallUpdateFields::new().title("Run cargo test"),
                ),
                vec![
                    acp::PermissionOption::new(
                        "allow-once",
                        "Allow",
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    acp::PermissionOption::new(
                        "reject-once",
                        "Reject",
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                ],
            )
        };
        let asked = async || {
            let AgentEvent::ApprovalRequested(request) = received.recv().await.unwrap() else {
                panic!("expected ApprovalRequested")
            };
            request.id
        };

        let (answer, id) = future::zip(
            client.request_permission(request(), future::pending()),
            async {
                let id = asked().await;
                cancel_pending(&client.state, &client.events).await;
                id
            },
        )
        .await;
        assert!(matches!(
            answer.unwrap().outcome,
            acp::RequestPermissionOutcome::Cancelled
        ));
        assert!(matches!(
            received.try_recv().unwrap(),
            AgentEvent::ApprovalResolved { request_id, decision: ApprovalDecision::Cancel }
                if request_id == id
        ));

        let (withdraw, withdrawal) = smol::channel::bounded::<()>(1);
        let (answer, id) = future::zip(
            client.request_permission(request(), async move {
                let _ = withdrawal.recv().await;
            }),
            async {
                let id = asked().await;
                withdraw.send(()).await.unwrap();
                id
            },
        )
        .await;
        assert!(answer.is_err());
        assert!(matches!(
            received.try_recv().unwrap(),
            AgentEvent::ApprovalResolved { request_id, decision: ApprovalDecision::Cancel }
                if request_id == id
        ));
    });
}

#[test]
fn approvals_classify_by_tool_kind() {
    let build = |kind: &str| {
        let tool: acp::ToolCallUpdate = serde_json::from_value(json!({
            "toolCallId": "t",
            "title": "Read config",
            "kind": kind,
            "rawInput": { "path": "a.rs" }
        }))
        .unwrap();
        approval_request("a1".into(), None, &tool, &[]).kind
    };
    assert!(matches!(build("read"), ApprovalKind::FileRead { .. }));
    assert!(matches!(build("other"), ApprovalKind::ToolUse { .. }));

    let edit: acp::ToolCallUpdate = serde_json::from_value(json!({
        "toolCallId": "t",
        "title": "Edit",
        "kind": "edit",
        "content": [{ "type": "diff", "path": "a.rs", "oldText": "a\n", "newText": "b\n" }]
    }))
    .unwrap();
    match approval_request("a2".into(), None, &edit, &[]).kind {
        ApprovalKind::FileChange { changes, .. } => assert_eq!(changes[0].path, "a.rs"),
        other => panic!("expected FileChange, got {other:?}"),
    }
}

#[test]
fn stop_reasons_map_to_turn_status() {
    assert_eq!(
        stop_reason_status(acp::StopReason::EndTurn).0,
        TurnStatus::Completed
    );
    assert_eq!(
        stop_reason_status(acp::StopReason::Cancelled).0,
        TurnStatus::Interrupted
    );
    let (status, message) = stop_reason_status(acp::StopReason::Refusal);
    assert_eq!(status, TurnStatus::Failed);
    assert!(message.is_some());
    let (status, message) = stop_reason_status(acp::StopReason::MaxTokens);
    assert_eq!(status, TurnStatus::Failed);
    assert!(message.is_some());
}

#[test]
fn prompt_carries_text_and_image_blocks() {
    let blocks = prompt_blocks(
        "hello",
        &[Attachment {
            media_type: "image/png".into(),
            data_base64: "AAAA".into(),
            source_path: None,
        }],
        None,
    );
    let value = serde_json::to_value(&blocks).unwrap();
    assert_eq!(value[0]["type"], "text");
    assert_eq!(value[0]["text"], "hello");
    assert_eq!(value[1]["type"], "image");
    assert_eq!(value[1]["mimeType"], "image/png");
    assert_eq!(value[1]["data"], "AAAA");
}
