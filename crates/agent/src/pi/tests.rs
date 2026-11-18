use super::*;
use crate::TurnStatus;

#[test]
fn responds_to_pending_extension_dialog_with_value_or_cancellation() {
    let mut approvals = HashSet::from(["confirm".to_owned()]);
    let mut dialogs = HashSet::from(["dialog".to_owned()]);
    let mut output = Vec::new();
    let dismissed = dismiss_prompts(&mut output, &mut approvals, &mut dialogs).unwrap();
    assert!(approvals.is_empty() && dialogs.is_empty());
    let responses: Vec<Value> = serde_json::Deserializer::from_slice(&output)
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        responses,
        [
            json!({"type":"extension_ui_response","id":"confirm","confirmed":false}),
            json!({"type":"extension_ui_response","id":"dialog","cancelled":true}),
        ]
    );
    assert!(
        matches!(dismissed.as_slice(), [
                AgentEvent::ApprovalResolved { request_id: approval, decision: ApprovalDecision::Cancel },
                AgentEvent::UserInputResolved { request_id: dialog, answers },
            ] if approval == "confirm" && dialog == "dialog" && answers.is_empty()),
        "{dismissed:?}"
    );

    for (answer, response) in [
        (
            Some(json!("custom answer")),
            json!({"type":"extension_ui_response","id":"dialog","value":"custom answer"}),
        ),
        (
            None,
            json!({"type":"extension_ui_response","id":"dialog","cancelled":true}),
        ),
    ] {
        let mut pending = HashSet::from(["dialog".to_owned()]);
        let answers = serde_json::Map::from_iter(answer.map(|answer| ("dialog".into(), answer)));
        let mut output = Vec::new();
        let resolved =
            answer_dialog(&mut output, &mut pending, "dialog".into(), answers.clone()).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), response);
        assert!(
            matches!(resolved, Some(AgentEvent::UserInputResolved { ref request_id, answers: ref resolved })
                    if request_id == "dialog" && *resolved == answers),
            "{resolved:?}"
        );
        assert!(pending.is_empty());
    }
    let mut output = Vec::new();
    assert!(
        answer_dialog(
            &mut output,
            &mut HashSet::new(),
            "dialog".into(),
            serde_json::Map::new()
        )
        .unwrap()
        .is_none()
    );
    assert!(output.is_empty());
}

/// The `notify` request of pi 1.5.1's `docs/rpc-extension-ui.md`, whose
/// `notifyType` is "info", "warning" or "error", "info" when omitted.
#[test]
fn a_notification_reaches_the_user() {
    let notify = json!({
        "type":"extension_ui_request",
        "id":"1",
        "method":"notify",
        "message":"my-extension synced 3 files"
    });
    assert_eq!(
        extension_notice(&notify).as_deref(),
        Some("my-extension synced 3 files")
    );
    assert!(
            extension_notice(&json!({
                "type":"extension_ui_request","id":"1","method":"setStatus","statusKey":"sync","statusText":"busy"
            }))
            .is_none()
        );
}

/// The `select`, `input` and `editor` requests of pi 1.5.1's
/// `docs/rpc-extension-ui.md`.
#[test]
fn maps_extension_dialog_requests_to_native_user_input() {
    let (id, select) = extension_dialog_question(&json!({
        "type":"extension_ui_request",
        "id":"select-id",
        "method":"select",
        "title":"Choose",
        "options":["a","b"],
        "timeout":10000
    }))
    .unwrap();
    assert_eq!(id, "select-id");
    assert_eq!(select.id, "select-id");
    assert_eq!(select.header, "pi");
    assert_eq!(select.question, "Choose");
    assert!(!select.multi_select);
    assert_eq!(select.prefill, None);
    assert_eq!(
        select
            .options
            .iter()
            .map(|option| (option.label.clone(), option.description.clone()))
            .collect::<Vec<_>>(),
        vec![("a".into(), String::new()), ("b".into(), String::new())]
    );

    let (_, input) = extension_dialog_question(&json!({
        "type":"extension_ui_request",
        "id":"input-id",
        "method":"input",
        "title":"Name",
        "placeholder":"type here"
    }))
    .unwrap();
    assert_eq!(input.question, "Name (type here)");
    assert!(input.options.is_empty());
    assert_eq!(input.prefill, None);

    let (_, editor) = extension_dialog_question(&json!({
        "type":"extension_ui_request",
        "id":"editor-id",
        "method":"editor",
        "title":"Edit",
        "prefill":"initial text"
    }))
    .unwrap();
    assert_eq!(editor.question, "Edit");
    assert!(editor.options.is_empty());
    assert_eq!(editor.prefill.as_deref(), Some("initial text"));
}

/// A model as pi 1.5.1's `docs/models-configuration.md` defines it, with
/// its `thinkingLevelMap`.
#[test]
fn model_catalog_only_offers_and_defaults_to_supported_thinking_levels() {
    let model = json!({
        "id": "test-model",
        "provider": "test-provider",
        "reasoning": false,
        "thinkingLevelMap": {"xhigh": "xhigh", "max": "max"}
    });
    assert!(map_model(&model, None, None).unwrap().options.is_empty());

    // Pi's thinkingLevelMap contract: omitted keys use provider defaults,
    // null disables a level, and xhigh/max require explicit support.
    for (mapping, expected) in [
        (json!(null), vec!["off", "minimal", "low", "medium", "high"]),
        (
            json!({"low": "low"}),
            vec!["off", "minimal", "low", "medium", "high"],
        ),
        (
            json!({"minimal": null, "medium": null, "xhigh": null, "max": "max"}),
            vec!["off", "low", "high", "max"],
        ),
        (
            json!({"off": null, "xhigh": "xhigh", "max": null}),
            vec!["minimal", "low", "medium", "high", "xhigh"],
        ),
    ] {
        let mut model = json!({
            "id": "test-model",
            "provider": "test-provider",
            "reasoning": true
        });
        if !mapping.is_null() {
            model["thinkingLevelMap"] = mapping;
        }
        for selected in ["medium", "xhigh", "unsupported"] {
            let mapped = map_model(&model, None, Some(selected)).unwrap();
            let [
                OptionDescriptor::Select {
                    options,
                    default_value,
                    ..
                },
            ] = mapped.options.as_slice()
            else {
                panic!("reasoning model must expose its supported thinking levels");
            };
            assert_eq!(
                options
                    .iter()
                    .map(|option| option.value.as_str())
                    .collect::<Vec<_>>(),
                expected,
                "{model}"
            );
            assert_eq!(
                default_value.as_deref(),
                expected.contains(&selected).then_some(selected)
            );
        }
    }
}

/// `CustomMessage` of pi 1.5.1's `docs/message-types.md`: `customType`,
/// `content` (text or text and image parts) and `display` are always set.
#[test]
fn displayed_custom_messages_become_work_log_items() {
    let mut mapper = PiMapper::new();
    for message in [
        json!({"role":"custom","customType":"my-extension","content":"hidden","display":false}),
        json!({"role":"custom","customType":"my-extension","content":[{"type":"image","data":"ignored"}],"display":true}),
    ] {
        assert!(
            mapper
                .on_message(&json!({"type":"message_end","message":message}))
                .is_empty()
        );
    }

    let plain = mapper.on_message(&json!({
        "type":"message_end",
        "message":{
            "role":"custom",
            "customType":"my-extension",
            "content":"plain text",
            "display":true,
            "timestamp":1785400000000_u64
        }
    }));
    assert!(matches!(
        plain.as_slice(),
        [AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::Other { provider_kind, summary },
            ..
        })] if provider_kind == "my-extension" && summary == "plain text"
    ));

    let parts = mapper.on_message(&json!({
        "type":"message_end",
        "message":{
            "role":"custom",
            "customType":"my-extension",
            "content":[
                {"type":"text","text":"first"},
                {"type":"image","data":"ignored"},
                {"type":"text","text":"second"}
            ],
            "display":true,
            "timestamp":1785400000001_u64
        }
    }));
    assert!(matches!(
        parts.as_slice(),
        [AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::Other { provider_kind, summary },
            ..
        })] if provider_kind == "my-extension" && summary == "first\nsecond"
    ));
}

/// A provider request that failed, as pi 1.5.1 reports it: pi-agent-core's
/// `streamAssistantResponse` turns the stream's `error` event into the
/// `message_end` of an `AssistantMessage` (pi-ai `dist/types.d.ts`) whose
/// `stopReason` is "error" and whose `errorMessage` the provider set
/// (e.g. pi-ai `dist/api/openai-responses.js`), and the run ends.
#[test]
fn a_failed_provider_request_reaches_the_user_with_its_reason() {
    let mut mapper = PiMapper::new();
    let message = json!({
        "role":"assistant",
        "content":[],
        "api":"openai-responses",
        "provider":"openai",
        "model":"gpt-test",
        "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
                 "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},
        "stopReason":"error",
        "errorMessage":"429 rate limit reached for gpt-test",
        "timestamp":1785400000000_u64
    });
    let mut events = Vec::new();
    for line in [
        json!({"type":"agent_start"}),
        json!({"type":"message_start","message":message}),
        json!({"type":"message_end","message":message}),
        json!({"type":"turn_end","message":message,"toolResults":[]}),
        json!({"type":"agent_end","messages":[message]}),
        json!({"type":"agent_settled","aborted":false}),
    ] {
        events.extend(mapper.on_message(&line));
    }
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Error { message, fatal: false }
            if message == "429 rate limit reached for gpt-test"
    )));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            status: TurnStatus::Failed,
            ..
        })
    ));
}

/// A failed request pi retried and then got an answer for, as pi 1.5.2
/// reports it (`dist/core/agent-session.js`): the failed attempt's
/// `message_end`, `auto_retry_start` before it continues the run, and
/// `auto_retry_end` with `success: true` once an assistant message ends
/// without an error. The failed attempt's `agent_end` says `willRetry`, so
/// its reason is told with the retry and is no error of the turn, which
/// completed.
#[test]
fn a_request_that_succeeds_on_retry_completes_its_turn_without_an_error() {
    let mut mapper = PiMapper::new();
    let usage = json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
                           "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}});
    let failed = json!({
        "role":"assistant","content":[],"api":"openai-responses","provider":"openai",
        "model":"gpt-test","usage":usage,"stopReason":"error",
        "errorMessage":"429 rate limit reached for gpt-test","timestamp":1785400000000_u64
    });
    let answered = json!({
        "role":"assistant","content":[{"type":"text","text":"Done."}],
        "api":"openai-responses","provider":"openai","model":"gpt-test","usage":usage,
        "stopReason":"stop","timestamp":1785400002000_u64
    });
    let mut events = Vec::new();
    for line in [
        json!({"type":"agent_start"}),
        json!({"type":"message_start","message":failed}),
        json!({"type":"message_end","message":failed}),
        json!({"type":"turn_end","message":failed,"toolResults":[]}),
        json!({"type":"agent_end","messages":[failed],"willRetry":true}),
        json!({"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":2000,
                   "errorMessage":"429 rate limit reached for gpt-test"}),
        json!({"type":"agent_start"}),
        json!({"type":"message_start","message":answered}),
        json!({"type":"message_end","message":answered}),
        json!({"type":"auto_retry_end","success":true,"attempt":1}),
        json!({"type":"turn_end","message":answered,"toolResults":[]}),
        json!({"type":"agent_end","messages":[answered]}),
        json!({"type":"agent_settled","aborted":false}),
    ] {
        events.extend(mapper.on_message(&line));
    }
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::RequestRetry { message }
            if message.contains("429 rate limit reached for gpt-test")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Error { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        })
    ));
}

/// Replays `pi --mode rpc` output recorded from pi 0.85.1 with a scripted
/// provider: one prompt, an assistant message that thinks and calls
/// `bash`, then a second assistant message that thinks and answers. The
/// two messages' `usage.cost` is invented, in the shape of `Usage` in
/// pi-ai 1.5.2's `dist/types.d.ts`.
fn recorded_rpc_events() -> Vec<Value> {
    include_str!("../../tests/fixtures/pi/rpc_events.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn streamed_text(events: &[AgentEvent], kind: DeltaKind) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Delta {
                kind: delta_kind,
                text,
                ..
            } if *delta_kind == kind => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Each `result` is what pi 1.4.0's own `bash`, `edit`, `write` and
/// `read` tools returned in a scratch directory (`createToolDefinition(..)
/// .execute`), in the `tool_execution_*` events `pi --mode rpc` forwards
/// verbatim. The items keep bash's exit code, edit's unified patch, the
/// written text and the read image; the failed bash fails its own item,
/// not the turn pi finished.
#[test]
fn tool_results_keep_exit_code_patch_and_images() {
    let mut mapper = PiMapper::new();
    let mut completed = Vec::new();
    mapper.on_message(&json!({"type":"agent_start"}));
    for line in include_str!("../../tests/fixtures/pi/tool_results.jsonl").lines() {
        for event in mapper.on_message(&serde_json::from_str(line).unwrap()) {
            if let AgentEvent::ItemCompleted(item) = event {
                completed.push(item.content);
            }
        }
    }
    assert!(matches!(
        mapper
            .on_message(&json!({"type":"agent_settled","aborted":false}))
            .as_slice(),
        [AgentEvent::TurnCompleted {
            status: TurnStatus::Completed,
            ..
        }]
    ));
    assert_eq!(
            completed,
            [
                ItemContent::CommandExecution {
                    command: "echo hi; exit 3".into(),
                    input: Vec::new(),
                    output: "hi\n\n\nCommand exited with code 3".into(),
                    exit_code: Some(3),
                    status: ItemStatus::Failed,
                },
                ItemContent::FileChange {
                    changes: vec![FileChange {
                        path: "a.txt".into(),
                        kind: FileChangeKind::Modify,
                        diff: Some(
                            "--- a.txt\n+++ a.txt\n@@ -1,2 +1,2 @@\n-hello\n+bye\n world\n".into()
                        ),
                    }],
                    status: ItemStatus::Completed,
                    output: Some("Successfully replaced 1 block(s) in a.txt.".into()),
                    command: None,
                },
                ItemContent::FileChange {
                    changes: vec![FileChange {
                        path: "new.txt".into(),
                        kind: FileChangeKind::Create,
                        diff: Some("+fresh".into()),
                    }],
                    status: ItemStatus::Completed,
                    output: Some("Successfully wrote to new.txt".into()),
                    command: None,
                },
                ItemContent::ToolCall {
                    image_reads: vec![Attachment {
                        media_type: "image/png".into(),
                        data_base64: "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEElEQVR4nGNgYPiPAw0pCQCpcD/BFMrqcwAAAABJRU5ErkJggg==".into(),
                        source_path: None,
                    }],
                    name: "read".into(),
                    input: json!({"path": "blue.png"}),
                    output: Some("Read image file [image/png]".into()),
                    status: ItemStatus::Completed,
                    family: Some(Box::new(ToolFamily::Read {
                        path: "blue.png".into(),
                        first_line: None,
                        note: None,
                    })),
                },
            ]
        );
}

/// `read` returns the lines as they are, from its `offset`, and when
/// its `limit` stopped short of the file's end, a blank line and a notice
/// of what is left (1.5.2's `dist/core/tools/read.js`; content invented).
#[test]
fn a_read_is_the_lines_it_read_from_its_offset() {
    assert_eq!(
        tool_family(
            "read",
            &json!({"path": "src/lib.rs", "offset": 40, "limit": 2}),
            Some("fn main() {\n}\n\n[48 more lines in file. Use offset=42 to continue.]".into()),
            None,
            ItemStatus::Completed,
        ),
        (
            Some(ToolFamily::Read {
                path: "src/lib.rs".into(),
                first_line: Some(40),
                note: Some("[48 more lines in file. Use offset=42 to continue.]".into()),
            }),
            Some("fn main() {\n}".into()),
        )
    );
}

/// grep names a match `path:N: text` under the directory it searched,
/// a context line `path-N- text`, and its limits in a `[…]` notice after
/// a blank line; ls lists the names in its directory (1.5.2's
/// `dist/core/tools/grep.js` and `ls.js`; content invented).
// The fixture's paths are as the harness prints them on a POSIX machine.
#[cfg(unix)]
#[test]
fn a_search_is_what_it_found_under_where_it_looked() {
    let (family, _) = tool_family(
            "grep",
            &json!({"pattern": "TODO", "path": "src"}),
            Some("a.rs-2- fn a() {\na.rs:3: // TODO: one\n\n[100 matches limit reached. Use limit=200 for more, or refine pattern]".into()),
            None,
            ItemStatus::Completed,
        );
    assert_eq!(
        family,
        Some(ToolFamily::Search {
            scope: SearchScope::Content,
            pattern: Some("TODO".into()),
            path: Some("src".into()),
            glob: None,
            hits: Some(vec![SearchHit {
                path: "src/a.rs".into(),
                line: Some(3),
                text: Some("// TODO: one".into()),
            }]),
        })
    );
    let (family, _) = tool_family(
        "ls",
        &json!({"path": "src"}),
        Some("(empty directory)".into()),
        None,
        ItemStatus::Completed,
    );
    assert!(matches!(
        family,
        Some(ToolFamily::Search { scope: SearchScope::Entries, hits: Some(hits), .. }) if hits.is_empty()
    ));
}

/// The tool_search extension names the tools it loaded in
/// `details.loaded` (1.5.2's `dist/extensions/tool-search/tool.js`;
/// content invented).
#[test]
fn a_tool_search_is_the_tools_it_loaded() {
    let (family, _) = tool_family(
        "tool_search",
        &json!({"query": "browser"}),
        Some(
            "Loaded 1 tool. They are available from your next call:\n- browser_open: Open a page"
                .into(),
        ),
        Some(&json!({"content": [], "details": {"loaded": ["browser_open"]}})),
        ItemStatus::Completed,
    );
    assert_eq!(
        family,
        Some(ToolFamily::LoadTools {
            query: "browser".into(),
            tools: Some(vec!["browser_open".into()]),
        })
    );
}

#[test]
fn maps_recorded_rpc_fixture() {
    let mut mapper = PiMapper::new();
    let mut events = Vec::new();
    for message in recorded_rpc_events() {
        events.extend(mapper.on_message(&message));
    }
    let mut streamed: Vec<(String, DeltaKind)> = Vec::new();
    for event in &events {
        if let AgentEvent::Delta { item_id, kind, .. } = event
            && !streamed.iter().any(|(id, _)| id == item_id)
        {
            streamed.push((item_id.clone(), *kind));
        }
    }
    let streamed_ids: Vec<&str> = streamed.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        streamed_ids,
        [
            "pi-assistant-1789592281572:0",
            "pi-assistant-1789592281595:0",
            "pi-assistant-1789592281595:1"
        ],
        "each assistant message streams under its own id"
    );
    for (item_id, kind) in &streamed {
        let completed: Vec<&ItemContent> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ItemCompleted(item) if item.id == *item_id => Some(&item.content),
                _ => None,
            })
            .collect();
        assert_eq!(completed.len(), 1, "{item_id} completes exactly once");
        assert!(
            matches!(
                (completed[0], kind),
                (ItemContent::Reasoning { .. }, DeltaKind::ReasoningText)
                    | (
                        ItemContent::AssistantMessage { .. },
                        DeltaKind::AssistantText
                    )
            ),
            "{item_id} completes as the kind it streamed"
        );
    }
    assert!(
            !events.iter().any(|event| matches!(
                event,
                AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::Reasoning { .. } | ItemContent::AssistantMessage { .. }, .. })
                    if !streamed_ids.contains(&id.as_str())
            )),
            "no assistant part completes under an id that was never streamed"
        );

    assert!(matches!(events[0], AgentEvent::TurnStarted { .. }));
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::ItemCompleted(ThreadItem {
            content: ItemContent::UserMessage { text, .. },
            ..
        }) if text == "DO NOT ECHO"
    )));
    assert_eq!(streamed_text(&events, DeltaKind::AssistantText), "PONG");
    assert_eq!(
        streamed_text(&events, DeltaKind::ReasoningText),
        "CheckingTool done"
    );
    assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemUpdated(ThreadItem { content: ItemContent::CommandExecution { output, .. }, .. }) if output == "ok"
        )));
    assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::CommandExecution { command, output, .. }, .. })
                if command == "printf ok" && output == "ok"
        )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TokenUsage(TokenUsage {
            input_tokens: Some(12),
            output_tokens: Some(3),
            ..
        })
    )));
    // The turn's two assistant messages used 19 and 27 tokens and cost
    // 0.25 and 0.5; each is reported by its `message_end` and again by
    // its `turn_end`. The turn processed and cost what both did; the
    // context holds what the last one counted.
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Some(TokenUsage {
                used_tokens: Some(27),
                total_processed_tokens: Some(46),
                cost_usd: Some(cost),
                ..
            }),
            ..
        }) if *cost == 0.75
    ));
}
