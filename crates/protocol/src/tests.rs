use std::path::PathBuf;

use agent::AgentEvent;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;

use super::*;

#[test]
fn client_ndjson_preserves_ids_text_and_record_boundaries() {
    let message = ClientMessage {
        id: u64::MAX,
        payload: ClientPayload::Command(Command::SendTurn {
            session_id: "session-1".into(),
            text: "第一行\n\"quoted\"".into(),
            attachment_paths: vec![PathBuf::from("/tmp/image.png")],
        }),
    };
    let line = encode_line(&message).unwrap();
    assert!(line.ends_with('\n'));
    assert_eq!(line.lines().count(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap(),
        json!({
            "id": u64::MAX,
            "payload": {
                "type": "command",
                "content": {
                    "type": "send_turn",
                    "content": {
                        "session_id": "session-1",
                        "text": "第一行\n\"quoted\"",
                        "attachment_paths": ["/tmp/image.png"],
                    },
                },
            },
        })
    );
    assert_eq!(decode_client_line(&line).unwrap(), message);
}

#[test]
fn host_responses_preserve_errors_and_correlation() {
    let error = ProtocolError {
        code: "not_found".into(),
        message: "missing session".into(),
    };
    for (message, kind) in [
        (
            HostMessage::Ack {
                id: 7,
                result: Err(error.clone()),
            },
            "ack",
        ),
        (
            HostMessage::QueryResult {
                id: 7,
                result: Err(error),
            },
            "query_result",
        ),
    ] {
        let line = encode_line(&message).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            json!({
                "type": kind,
                "content": {
                    "id": 7,
                    "result": {"Err": {"code": "not_found", "message": "missing session"}},
                },
            })
        );
        assert_eq!(decode_host_line(&line).unwrap(), message);
    }
}

#[test]
fn event_envelopes_keep_stored_record_shape_and_optional_request_id() {
    let mut envelope = EventEnvelope {
        request_id: None,
        topic: Topic::SessionEvents {
            session_id: "session-1".into(),
        },
        event: ServerEvent::SessionEvent(SessionEventRecord {
            ts: Some(123),
            event: AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
        }),
    };
    let expected = json!({
        "topic": {"type": "session_events", "content": {"session_id": "session-1"}},
        "event": {
            "type": "session_event",
            "content": {"ts": 123, "event": {"type": "turn_started", "turn_id": "turn-1"}},
        },
    });
    assert_eq!(serde_json::to_value(&envelope).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<EventEnvelope>(expected).unwrap(),
        envelope
    );

    envelope.request_id = Some(42);
    let message = HostMessage::Event(envelope);
    let line = encode_line(&message).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["content"]["request_id"],
        42
    );
    assert_eq!(decode_host_line(&line).unwrap(), message);
}

fn assert_binary_wire<T>(message: T, pointer: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let mut value = serde_json::to_value(&message).unwrap();
    assert_eq!(value.pointer(pointer).unwrap(), "AP8KGw==");
    assert_eq!(serde_json::from_value::<T>(value.clone()).unwrap(), message);
    *value.pointer_mut(pointer).unwrap() = json!("not base64!");
    assert!(serde_json::from_value::<T>(value).is_err());
}

#[test]
fn binary_payloads_use_base64_and_reject_corrupt_input() {
    let bytes = vec![0, 0xff, b'\n', b'\x1b'];
    assert_binary_wire(
        Command::TerminalInput {
            terminal_id: 9,
            bytes: bytes.clone(),
        },
        "/content/bytes",
    );
    assert_binary_wire(
        Query::SaveAttachment {
            dir: PathBuf::from("/tmp/attachments"),
            bytes: bytes.clone(),
            ext: "png".into(),
        },
        "/content/bytes",
    );
    assert_binary_wire(
        ServerEvent::TerminalOutput {
            terminal_id: 9,
            bytes: bytes.clone(),
            reset: true,
            cols: 80,
            rows: 24,
        },
        "/content/bytes",
    );
    assert_binary_wire(QueryResponse::FileBytes(bytes), "/content");
}

#[test]
fn older_messages_default_new_optional_fields() {
    let subscription: Subscription =
        serde_json::from_value(json!({"topic": {"type": "index"}})).unwrap();
    assert_eq!(
        subscription,
        Subscription {
            topic: Topic::Index,
            after: None
        }
    );
    let index: IndexSnapshot =
        serde_json::from_value(json!({"sessions": [], "projects": []})).unwrap();
    assert!(index.activity.is_empty());
    let queued: QueuedMessageStatus =
        serde_json::from_value(json!({"id": 1, "text": "next"})).unwrap();
    assert_eq!(queued.fire_at_unix_secs, None);
    let command = decode_client_line(r#"{"id":1,"payload":{"type":"command","content":{"type":"capture_terminal_selection","content":{"session_id":"s","terminal_id":9}}}}"#).unwrap();
    assert!(matches!(
        command.payload,
        ClientPayload::Command(Command::CaptureTerminalSelection {
            selection: None,
            ..
        })
    ));
}

/// Import progress and content search are replicated host state and a host-run
/// query. Their literal shapes are the contract a non-Rust or older client
/// depends on, so assert the JSON rather than a round trip.
#[test]
fn import_status_and_content_search_use_their_documented_wire_shapes() {
    let start = ClientMessage {
        id: 3,
        payload: ClientPayload::Command(Command::StartExternalImport {
            project_id: "project-1".into(),
            threads: vec![ExternalThread {
                source: SourceTool::ClaudeCode,
                file: PathBuf::from("/history/a.jsonl"),
                external_id: "claude:abc".into(),
                title_hint: None,
                last_active_ms: 17,
            }],
        }),
    };
    assert_eq!(
        serde_json::to_value(&start).unwrap(),
        json!({
            "id": 3,
            "payload": {
                "type": "command",
                "content": {
                    "type": "start_external_import",
                    "content": {
                        "project_id": "project-1",
                        "threads": [{
                            "source": "claude_code",
                            "file": "/history/a.jsonl",
                            "external_id": "claude:abc",
                            "title_hint": null,
                            "last_active_ms": 17,
                        }],
                    },
                },
            },
        })
    );
    assert_eq!(
        decode_client_line(&encode_line(&start).unwrap()).unwrap(),
        start
    );

    assert_eq!(
        serde_json::to_value(CommandResponse::ExternalImportStarted(false)).unwrap(),
        json!({"type": "external_import_started", "content": false})
    );

    for (status, expected) in [
        (None, json!(null)),
        (
            Some(ExternalImportStatus {
                run_id: 4,
                state: ExternalImportState::Progress {
                    done: 1,
                    total: 2,
                    tool: "Codex CLI".into(),
                },
            }),
            json!({
                "run_id": 4,
                "state": {
                    "type": "progress",
                    "content": {"done": 1, "total": 2, "tool": "Codex CLI"},
                },
            }),
        ),
        (
            Some(ExternalImportStatus {
                run_id: 4,
                state: ExternalImportState::Finished {
                    imported: 2,
                    skipped: 1,
                },
            }),
            json!({
                "run_id": 4,
                "state": {
                    "type": "finished",
                    "content": {"imported": 2, "skipped": 1},
                },
            }),
        ),
    ] {
        let envelope = EventEnvelope {
            request_id: None,
            topic: Topic::ExternalImport {
                project_id: "project-1".into(),
            },
            event: ServerEvent::ExternalImportStatusReplaced {
                project_id: "project-1".into(),
                status,
            },
        };
        let value = json!({
            "topic": {"type": "external_import", "content": {"project_id": "project-1"}},
            "event": {
                "type": "external_import_status_replaced",
                "content": {"project_id": "project-1", "status": expected},
            },
        });
        assert_eq!(serde_json::to_value(&envelope).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<EventEnvelope>(value).unwrap(),
            envelope
        );
    }

    let search = ClientMessage {
        id: 9,
        payload: ClientPayload::Query(Query::SearchSessionContent {
            query: "auth.rs".into(),
            limit: 50,
        }),
    };
    assert_eq!(
        serde_json::to_value(&search).unwrap(),
        json!({
            "id": 9,
            "payload": {
                "type": "query",
                "content": {
                    "type": "search_session_content",
                    "content": {"query": "auth.rs", "limit": 50},
                },
            },
        })
    );
    assert_eq!(
        decode_client_line(&encode_line(&search).unwrap()).unwrap(),
        search
    );

    let hits = QueryResponse::SessionContentHits(vec![SessionSearchHit {
        session_id: "session-1".into(),
        session_title: "Authentication cleanup".into(),
        entry_id: "entry-1".into(),
        turn: 2,
        snippet: "…updated auth.rs…".into(),
    }]);
    let hits_value = json!({
        "type": "session_content_hits",
        "content": [{
            "session_id": "session-1",
            "session_title": "Authentication cleanup",
            "entry_id": "entry-1",
            "turn": 2,
            "snippet": "…updated auth.rs…",
        }],
    });
    assert_eq!(serde_json::to_value(&hits).unwrap(), hits_value);
    assert_eq!(
        serde_json::from_value::<QueryResponse>(hits_value).unwrap(),
        hits
    );
}

#[test]
fn unknown_data_variants_and_malformed_records_return_decode_errors() {
    for line in [
        "",
        "not JSON",
        r#"{"id":7,"payload":{"type":"command","content":{"type":"future_command","content":{"value":1}}}}"#,
        r#"{"id":7,"payload":{"type":"future_payload"}}"#,
    ] {
        assert_eq!(decode_client_line(line).unwrap_err().code, "decode_error");
    }
    for line in [
        "{",
        r#"{"type":"future_host_message","content":{}}"#,
        r#"{"type":"event","content":{"topic":{"type":"index"},"event":{"type":"future_event","content":{}}}}"#,
    ] {
        assert_eq!(decode_host_line(line).unwrap_err().code, "decode_error");
    }
    assert_eq!(
        serde_json::from_value::<GitDiffScope>(json!("future_scope")).unwrap(),
        GitDiffScope::Unknown
    );
    assert_eq!(
        serde_json::from_value::<SourceTool>(json!("future_tool")).unwrap(),
        SourceTool::Unknown
    );
}
