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
        crate::terminal::TerminalImage {
            key: 3,
            width: 1,
            height: 1,
            color: crate::terminal::ImageColorType::Rgba,
            pixels: bytes.clone(),
        },
        "/pixels",
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

/// The replicated terminal grid is the whole client contract, so its literal
/// shape is asserted rather than round-tripped. Everything that is at its
/// default is omitted: a blank cell is `{}` and a blank row is `{}`.
#[test]
fn terminal_frame_wire_shape_omits_defaults() {
    use crate::terminal::{
        CellWidth, CursorShape, TerminalCell, TerminalColor, TerminalCursor, TerminalFrame,
        TerminalLink, TerminalModes, TerminalRow, TerminalStyle,
    };

    let frame = TerminalFrame {
        cols: 4,
        rows: 2,
        modes: TerminalModes {
            mode: 0b1_0000_0001_0001,
            keyboard: 1,
            modify_other_keys: Some(2),
        },
        cursor: Some(TerminalCursor {
            row: 1,
            col: 2,
            shape: CursorShape::Beam,
            blinking: true,
        }),
        styles: vec![
            TerminalStyle::default(),
            TerminalStyle {
                fg: TerminalColor::Rgb { r: 1, g: 2, b: 3 },
                bg: TerminalColor::Indexed(9),
                underline_color: None,
                flags: 0b110,
            },
        ],
        visible: vec![
            TerminalRow {
                cells: vec![
                    TerminalCell {
                        text: "中".into(),
                        width: CellWidth::Wide,
                        style: 1,
                        link: None,
                    },
                    TerminalCell {
                        width: CellWidth::Spacer,
                        ..TerminalCell::default()
                    },
                    TerminalCell {
                        text: "e\u{301}".into(),
                        link: Some(TerminalLink {
                            uri: "https://example.com".into(),
                            id: None,
                        }),
                        ..TerminalCell::default()
                    },
                ],
                wrapped: true,
            },
            TerminalRow::default(),
        ],
        history: vec![TerminalRow::default()],
        lines_evicted: 7,
        title: "zsh".into(),
        working_directory: Some(PathBuf::from("/tmp/project")),
        exited: false,
        exit_code: None,
        images: Vec::new(),
        overlays: Vec::new(),
    };
    let event = ServerEvent::TerminalFrame {
        terminal_id: 3,
        frame: Box::new(frame.clone()),
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        json!({
            "type": "terminal_frame",
            "content": {
                "terminal_id": 3,
                "frame": {
                    "cols": 4,
                    "rows": 2,
                    "modes": {"mode": 4113, "keyboard": 1, "modify_other_keys": 2},
                    "cursor": {"row": 1, "col": 2, "shape": "beam", "blinking": true},
                    "styles": [
                        {"fg": {"type": "foreground"}, "bg": {"type": "background"}},
                        {
                            "fg": {"type": "rgb", "content": {"r": 1, "g": 2, "b": 3}},
                            "bg": {"type": "indexed", "content": 9},
                            "flags": 6,
                        },
                    ],
                    "visible": [
                        {
                            "cells": [
                                {"text": "中", "width": "wide", "style": 1},
                                {"width": "spacer"},
                                {"text": "e\u{301}", "link": {"uri": "https://example.com"}},
                            ],
                            "wrapped": true,
                        },
                        {},
                    ],
                    "history": [{}],
                    "lines_evicted": 7,
                    "title": "zsh",
                    "working_directory": "/tmp/project",
                },
            },
        })
    );
    assert_eq!(
        serde_json::from_value::<ServerEvent>(json!({
            "type": "terminal_frame",
            "content": {"terminal_id": 3, "frame": serde_json::to_value(&frame).unwrap()},
        }))
        .unwrap(),
        event
    );
}

#[test]
fn terminal_delta_wire_shape_carries_only_what_changed() {
    use crate::terminal::{
        TerminalCell, TerminalDelta, TerminalHistoryUpdate, TerminalModes, TerminalRow,
        TerminalRowUpdate, TerminalStyle,
    };

    let delta = TerminalDelta {
        cols: 80,
        rows: 24,
        modes: TerminalModes::default(),
        cursor: None,
        lines_evicted: 0,
        styles: vec![TerminalStyle::default()],
        rows_replaced: vec![TerminalRowUpdate {
            index: 5,
            row: TerminalRow {
                cells: vec![TerminalCell {
                    text: "y".into(),
                    ..TerminalCell::default()
                }],
                wrapped: false,
            },
        }],
        history: Some(TerminalHistoryUpdate::Appended(
            vec![TerminalRow::default()],
        )),
        title: None,
        working_directory: None,
        exit: None,
        images_added: Vec::new(),
        images_removed: Vec::new(),
        overlays: None,
        bell: true,
        clipboard: None,
    };
    let event = ServerEvent::TerminalDelta {
        terminal_id: 3,
        delta: Box::new(delta.clone()),
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        json!({
            "type": "terminal_delta",
            "content": {
                "terminal_id": 3,
                "delta": {
                    "cols": 80,
                    "rows": 24,
                    "modes": {"mode": 0, "keyboard": 0},
                    "styles": [{"fg": {"type": "foreground"}, "bg": {"type": "background"}}],
                    "rows_replaced": [{"index": 5, "row": {"cells": [{"text": "y"}]}}],
                    "history": {"type": "appended", "content": [{}]},
                    "bell": true,
                },
            },
        })
    );
    assert_eq!(
        serde_json::from_value::<ServerEvent>(serde_json::to_value(&event).unwrap()).unwrap(),
        event
    );
}

/// A delta advances the frame exactly as the host's own retained projection
/// does: rows are replaced in place, history appends and trims at the cap, and
/// message-local style ids are remapped into the frame's own table.
#[test]
fn applying_a_delta_replaces_rows_and_trims_history_at_the_cap() {
    use crate::terminal::{
        HISTORY_LIMIT, TerminalCell, TerminalColor, TerminalDelta, TerminalFrame,
        TerminalHistoryUpdate, TerminalRow, TerminalRowUpdate, TerminalStyle,
    };

    let red = TerminalStyle {
        fg: TerminalColor::Indexed(1),
        ..TerminalStyle::default()
    };
    let mut frame = TerminalFrame {
        cols: 2,
        rows: 2,
        styles: vec![TerminalStyle::default()],
        visible: vec![TerminalRow::default(), TerminalRow::default()],
        history: (0..HISTORY_LIMIT).map(|_| TerminalRow::default()).collect(),
        ..TerminalFrame::default()
    };
    frame.apply(&TerminalDelta {
        cols: 2,
        rows: 2,
        // The delta's own table puts the new style at index 1.
        styles: vec![TerminalStyle::default(), red],
        rows_replaced: vec![TerminalRowUpdate {
            index: 1,
            row: TerminalRow {
                cells: vec![TerminalCell {
                    text: "x".into(),
                    style: 1,
                    ..TerminalCell::default()
                }],
                wrapped: false,
            },
        }],
        history: Some(TerminalHistoryUpdate::Appended(vec![TerminalRow {
            cells: vec![TerminalCell {
                text: "old".into(),
                ..TerminalCell::default()
            }],
            wrapped: false,
        }])),
        ..TerminalDelta::default()
    });

    assert!(frame.visible[0].cells.is_empty());
    assert_eq!(frame.style(&frame.visible[1].cells[0]), red);
    assert_eq!(frame.history.len(), HISTORY_LIMIT);
    assert_eq!(
        frame.history[HISTORY_LIMIT - 1].cells[0].text,
        "old",
        "the newest scrollback row is kept and the oldest dropped"
    );
}

/// An image quad wider than its pane is cut to the pane, and its source rect
/// shrinks with it so the visible slice is the covered part of the image.
#[test]
fn clipping_an_overlay_shrinks_its_source_rect_proportionally() {
    use crate::terminal::TerminalOverlay;

    let overlay = TerminalOverlay {
        image_key: 1,
        x: 0.,
        y: 0.,
        width: 100.,
        height: 40.,
        z_index: -1,
        source_rect: [0., 0., 1., 1.],
    };
    let clipped = overlay.clipped(25., 0., 75., 40.).unwrap();
    assert_eq!((clipped.x, clipped.width), (25., 50.));
    assert_eq!(clipped.source_rect, [0.25, 0., 0.75, 1.]);
    assert_eq!(overlay.clipped(200., 0., 300., 40.), None);
}
