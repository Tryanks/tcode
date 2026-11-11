use std::path::PathBuf;

use agent::AgentEvent;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;

use super::*;

/// The largest image a media read returns still goes out as one network line.
#[test]
fn the_largest_pull_request_image_fits_one_network_line() {
    let (mime, validator) = ("image/png", Some("\"etag-of-an-invented-image\""));
    let limit = max_pull_request_media_bytes(mime, validator);
    let answer = |bytes: usize| {
        encode_line(&HostMessage::QueryResult {
            id: u64::MAX,
            result: Ok(QueryResponse::PullRequest {
                response: Box::new(PullRequestReadResponse::Media(PullRequestMedia::Image {
                    bytes: vec![0; bytes],
                    mime: mime.into(),
                    validator: validator.map(str::to_owned),
                    expires_at: u64::MAX,
                })),
                expires_at: u64::MAX,
            }),
        })
        .unwrap()
        .len()
    };
    assert!(answer(limit) <= MAX_LINE_BYTES);
    assert!(answer(limit + 3) > MAX_LINE_BYTES);
}

#[test]
fn client_ndjson_preserves_ids_text_and_record_boundaries() {
    let message = ClientMessage {
        principal: None,
        key: None,
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
        event: ServerEvent::SessionEvent(tcode_core::session::StoredEvent {
            origin: None,
            author: None,
            ts: Some(123),
            event: AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            elided: None,
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

    if let ServerEvent::SessionEvent(record) = &mut envelope.event {
        record.author = Some(tcode_core::session::Author {
            device_id: "d".into(),
            name: "Alice".into(),
        });
    }
    assert_eq!(
        serde_json::to_value(&envelope).unwrap()["event"]["content"]["author"],
        json!({"device_id":"d","name":"Alice"})
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
    let value = serde_json::to_value(&message).unwrap();
    assert_eq!(value.pointer(pointer).unwrap(), "AP8KGw==");
    assert_eq!(serde_json::from_value::<T>(value).unwrap(), message);
}

#[test]
fn binary_payloads_use_base64() {
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
    assert_binary_wire(QueryResponse::FileBytes(bytes), "/content");
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
        scrolled: 1,
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
                    "scrolled": 1,
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
    let blue = TerminalStyle {
        fg: TerminalColor::Indexed(4),
        ..TerminalStyle::default()
    };
    let row = |text: String, style| TerminalRow {
        cells: vec![TerminalCell {
            text,
            style,
            ..TerminalCell::default()
        }],
        wrapped: false,
    };
    let mut frame = TerminalFrame {
        cols: 2,
        rows: 2,
        styles: vec![TerminalStyle::default(), blue],
        visible: vec![row("unchanged".into(), 1), TerminalRow::default()],
        history: (0..HISTORY_LIMIT)
            .map(|index| row(format!("old-{index}"), 0))
            .collect(),
        ..TerminalFrame::default()
    };
    frame.apply(&TerminalDelta {
        cols: 2,
        rows: 2,
        // Local style 1 is red here but blue in the retained frame.
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
        history: Some(TerminalHistoryUpdate::Appended(vec![row("new".into(), 0)])),
        ..TerminalDelta::default()
    });

    assert_eq!(frame.visible[0].cells[0].text, "unchanged");
    assert_eq!(frame.style(&frame.visible[0].cells[0]), blue);
    assert_eq!(frame.visible[1].cells[0].text, "x");
    assert_eq!(frame.style(&frame.visible[1].cells[0]), red);
    assert_eq!(frame.history.len(), HISTORY_LIMIT);
    assert_eq!(frame.history[0].cells[0].text, "old-1");
    assert_eq!(
        frame.history[HISTORY_LIMIT - 2].cells[0].text,
        format!("old-{}", HISTORY_LIMIT - 1)
    );
    assert_eq!(
        frame.history[HISTORY_LIMIT - 1].cells[0].text,
        "new",
        "the newest scrollback row is kept and the oldest dropped"
    );
}

/// The mux reads a subscription's topic and a pushed event's type from the
/// JSON itself, and keeps only the latest `command_output_replaced` of a
/// topic for a slow client.
#[test]
fn command_output_subscription_uses_its_documented_wire_shape() {
    use crate::terminal::{TerminalCell, TerminalFrame, TerminalRow, TerminalStyle};

    let topic = Topic::CommandOutput {
        session_id: "session-1".into(),
        item_id: "cmd-1".into(),
        cols: 40,
    };
    let request = ClientMessage {
        principal: None,
        key: None,
        id: 11,
        payload: ClientPayload::Subscribe(Subscription {
            known_index: None,
            topic: topic.clone(),
            after: None,
        }),
    };
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({
            "id": 11,
            "payload": {
                "type": "subscribe",
                "content": {
                    "topic": {
                        "type": "command_output",
                        "content": {"session_id": "session-1", "item_id": "cmd-1", "cols": 40},
                    },
                    "after": null,
                },
            },
        })
    );
    assert_eq!(
        decode_client_line(&encode_line(&request).unwrap()).unwrap(),
        request
    );

    // A rendered screen: no cursor, no scrollback, no session behind it.
    let pushed = HostMessage::Event(EventEnvelope {
        request_id: None,
        topic,
        event: ServerEvent::CommandOutputReplaced(Box::new(TerminalFrame {
            cols: 40,
            rows: 1,
            styles: vec![
                TerminalStyle::default(),
                TerminalStyle {
                    fg: crate::terminal::TerminalColor::Indexed(1),
                    flags: 0b10,
                    ..TerminalStyle::default()
                },
            ],
            visible: vec![TerminalRow {
                cells: vec![TerminalCell {
                    text: "x".into(),
                    style: 1,
                    ..TerminalCell::default()
                }],
                wrapped: false,
            }],
            ..TerminalFrame::default()
        })),
    });
    assert_eq!(
        serde_json::to_value(&pushed).unwrap(),
        json!({
            "type": "event",
            "content": {
                "topic": {
                    "type": "command_output",
                    "content": {"session_id": "session-1", "item_id": "cmd-1", "cols": 40},
                },
                "event": {
                    "type": "command_output_replaced",
                    "content": {
                        "cols": 40,
                        "rows": 1,
                        "modes": {"mode": 0, "keyboard": 0},
                        "styles": [
                            {"fg": {"type": "foreground"}, "bg": {"type": "background"}},
                            {
                                "fg": {"type": "indexed", "content": 1},
                                "bg": {"type": "background"},
                                "flags": 2,
                            },
                        ],
                        "visible": [{"cells": [{"text": "x", "style": 1}]}],
                        "title": "",
                    },
                },
            },
        })
    );
    assert_eq!(
        decode_host_line(&encode_line(&pushed).unwrap()).unwrap(),
        pushed
    );

    // A push to a subscriber holding a screen: a row that scrolled into the
    // history and the screen's last row.
    let changed = HostMessage::Event(EventEnvelope {
        request_id: None,
        topic: Topic::CommandOutput {
            session_id: "session-1".into(),
            item_id: "cmd-1".into(),
            cols: 40,
        },
        event: ServerEvent::CommandOutputDelta(Box::new(crate::terminal::TerminalDelta {
            cols: 40,
            rows: 16,
            styles: vec![TerminalStyle::default()],
            scrolled: 1,
            rows_replaced: vec![crate::terminal::TerminalRowUpdate {
                index: 15,
                row: TerminalRow {
                    cells: vec![TerminalCell {
                        text: "y".into(),
                        ..TerminalCell::default()
                    }],
                    wrapped: false,
                },
            }],
            history: Some(crate::terminal::TerminalHistoryUpdate::Appended(vec![
                TerminalRow::default(),
            ])),
            ..crate::terminal::TerminalDelta::default()
        })),
    });
    assert_eq!(
        serde_json::to_value(&changed).unwrap(),
        json!({
            "type": "event",
            "content": {
                "topic": {
                    "type": "command_output",
                    "content": {"session_id": "session-1", "item_id": "cmd-1", "cols": 40},
                },
                "event": {
                    "type": "command_output_delta",
                    "content": {
                        "cols": 40,
                        "rows": 16,
                        "modes": {"mode": 0, "keyboard": 0},
                        "styles": [{"fg": {"type": "foreground"}, "bg": {"type": "background"}}],
                        "scrolled": 1,
                        "rows_replaced": [{"index": 15, "row": {"cells": [{"text": "y"}]}}],
                        "history": {"type": "appended", "content": [{}]},
                    },
                },
            },
        })
    );
    assert_eq!(
        decode_host_line(&encode_line(&changed).unwrap()).unwrap(),
        changed
    );
}

#[test]
fn scope_subscriptions_and_provider_choices_have_literal_wire_contracts() {
    for (topic, wire) in [
        (Topic::Scope, json!({"type":"scope"})),
        (
            Topic::SpaceIndex {
                space_id: "s".into(),
            },
            json!({"type":"space_index","content":{"space_id":"s"}}),
        ),
        (Topic::Hosting, json!({"type":"hosting"})),
    ] {
        let wire = json!({"id":1,"principal":{"type":"full"},"payload":{"type":"subscribe","content":{"topic":wire,"after":null}}});
        let request = ClientMessage {
            id: 1,
            key: None,
            principal: Some(Principal::Full),
            payload: ClientPayload::Subscribe(Subscription {
                known_index: None,
                topic,
                after: None,
            }),
        };
        assert_eq!(serde_json::to_value(&request).unwrap(), wire);
        assert_eq!(decode_client_line(&wire.to_string()).unwrap(), request);
    }
    let scope = Scope::Space {
        space_id: "s".into(),
        space_name: "Shared".into(),
        projects: vec![],
        providers: vec![ScopedProviderChoice {
            provider: agent::ProviderKind::Codex,
            profile_id: Some("codex".into()),
            name: "Codex".into(),
            models: vec![agent::ModelSpec {
                id: "model".into(),
                display_name: "Model".into(),
                is_default: true,
                options: vec![],
            }],
        }],
        policy: SpacePolicy::new_space(),
    };
    let scope_wire = json!({"type":"space","content":{"space_id":"s","space_name":"Shared","projects":[],"providers":[{"provider":"codex","profile_id":"codex","name":"Codex","models":[{"id":"model","display_name":"Model","is_default":true,"options":[]}]}],
        "policy":{"agents":{"type":"only","content":[]},"approval":"no_unrestricted","terminals":false,"git_remote":false,"preview":false,"computer_use":false,"editor":false}}});
    for (event, wire) in [
        (
            ServerEvent::ScopeSnapshot(Scope::Full),
            json!({"type":"scope_snapshot","content":{"type":"full"}}),
        ),
        (
            ServerEvent::ScopeSnapshot(scope.clone()),
            json!({"type":"scope_snapshot","content":scope_wire.clone()}),
        ),
    ] {
        let response = HostMessage::Event(EventEnvelope {
            request_id: Some(1),
            topic: Topic::Scope,
            event,
        });
        let wire = json!({"type":"event","content":{"request_id":1,"topic":{"type":"scope"},"event":wire}});
        assert_eq!(serde_json::to_value(&response).unwrap(), wire);
        assert_eq!(decode_host_line(&wire.to_string()).unwrap(), response);
    }
}
