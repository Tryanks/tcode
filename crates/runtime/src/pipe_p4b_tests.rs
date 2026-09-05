//! P4b acceptance through serialized links and the production fan-out mux.
use super::*;
use std::time::Duration;
use tcode_remote::HostMux;

fn linked(mux: &HostMux) -> HostLink {
    let connection = mux.attach();
    let link = HostLink::new(connection.to_host, connection.from_host);
    let pump = link.clone();
    smol::spawn(async move { pump.pump().await }).detach();
    link
}

fn fixture() -> (SpawnedHost, HostMux, HostLink, String) {
    let root = std::env::temp_dir().join(format!("tcode-p4b-{}", uuid::Uuid::new_v4()));
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let host = spawn_host(
        SessionStore::open_at(root).unwrap(),
        HostServices::default(),
    )
    .unwrap();
    let mux = HostMux::new(host.to_host.clone(), host.from_host.clone());
    let link = linked(&mux);
    let CommandResponse::ProjectId(Some(project_id)) = link
        .command_blocking(Command::CreateProject {
            root: project.clone(),
        })
        .unwrap()
    else {
        panic!("no project");
    };
    let CommandResponse::SessionId(Some(session_id)) = link
        .command_blocking(Command::StartDraft {
            project_id,
            cwd: project,
        })
        .unwrap()
    else {
        panic!("no session");
    };
    link.subscribe(Subscription {
        topic: Topic::SessionStatus {
            session_id: session_id.clone(),
        },
        after: None,
    })
    .unwrap();
    (host, mux, link, session_id)
}

fn next(events: &HostEventReceiver, predicate: impl Fn(&ServerEvent) -> bool) -> ServerEvent {
    super::tests::next_event(events, |event| predicate(&event.event)).event
}

#[test]
fn terminal_mux_replays_bounded_raw_output_then_streams_input_and_resize() {
    let (host, mux, link, session_id) = fixture();
    let events = link.events();
    link.command_blocking(Command::ToggleTerminalPanel {
        session_id: session_id.clone(),
    })
    .unwrap();
    let ServerEvent::SessionStatusReplaced(status) = next(
        &events,
        |event| matches!(event, ServerEvent::SessionStatusReplaced(status) if !status.terminals.is_empty()),
    ) else {
        unreachable!()
    };
    let terminal_id = status.terminals[0].id;
    link.subscribe(Subscription {
        topic: Topic::Terminal { terminal_id },
        after: None,
    })
    .unwrap();
    // Replace the user's configured login shell (whose startup hooks may print
    // asynchronously) with plain sh, clear its prompt, then use the live mux
    // event as a barrier proving all startup output reached the ring.
    link.command_blocking(Command::TerminalInput {
        terminal_id,
        bytes: b"exec /bin/sh\rPS1=; printf '\\160\\064\\142-pty-ready\\n'\r".to_vec(),
    })
    .unwrap();
    let mut startup = Vec::new();
    while !String::from_utf8_lossy(&startup).contains("p4b-pty-ready") {
        if let ServerEvent::TerminalOutput { bytes, reset, .. } = next(&events, |event| {
            matches!(event, ServerEvent::TerminalOutput { .. })
        }) {
            assert!(!reset || startup.is_empty());
            startup.extend(bytes);
        }
    }
    link.unsubscribe(Subscription {
        topic: Topic::Terminal { terminal_id },
        after: None,
    })
    .unwrap();
    // Inject through the same mailbox callback as the output bridge, testing
    // byte-exact eviction without flooding the user's shell with 300 KiB.
    let replay = vec![b'x'; 300 * 1024];
    smol::block_on(host.update_state_for_test(move |state, cx| {
        state.emit_terminal_output(terminal_id, replay, false, cx)
    }))
    .unwrap();
    link.subscribe(Subscription {
        topic: Topic::Terminal { terminal_id },
        after: None,
    })
    .unwrap();
    let ServerEvent::TerminalOutput { bytes, reset, .. } = next(&events, |event| {
        matches!(event, ServerEvent::TerminalOutput { reset: true, .. })
    }) else {
        unreachable!()
    };
    assert!(reset);
    assert_eq!(bytes, vec![b'x'; 256 * 1024]);
    link.command_blocking(Command::ResizeTerminal {
        terminal_id,
        cols: 93,
        rows: 17,
    })
    .unwrap();
    assert_eq!(
        host.terminals
            .terminal(terminal_id)
            .unwrap()
            .grid()
            .dimensions(),
        (93, 17)
    );
    link.command_blocking(Command::TerminalInput {
        terminal_id,
        bytes: b"printf '\\160\\064\\142-STREAM\\n'; stty size; printf '\\160\\064\\142-live-done\\n'\r".to_vec(),
    })
    .unwrap();
    let mut live = Vec::new();
    while !String::from_utf8_lossy(&live).contains("p4b-STREAM")
        || !String::from_utf8_lossy(&live).contains("17 93")
        || !String::from_utf8_lossy(&live).contains("p4b-live-done")
    {
        if let ServerEvent::TerminalOutput { bytes, reset, .. } = next(&events, |event| {
            matches!(event, ServerEvent::TerminalOutput { .. })
        }) {
            assert!(!reset);
            live.extend(bytes);
        }
    }
    let other = linked(&mux);
    let other_events = other.events();
    other
        .subscribe(Subscription {
            topic: Topic::Terminal { terminal_id },
            after: None,
        })
        .unwrap();
    let ServerEvent::TerminalOutput { reset, bytes, .. } = next(&other_events, |event| {
        matches!(event, ServerEvent::TerminalOutput { .. })
    }) else {
        unreachable!()
    };
    assert!(reset);
    assert!(bytes.len() <= 256 * 1024);
    assert!(String::from_utf8_lossy(&bytes).contains("p4b-STREAM"));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event.event, ServerEvent::TerminalOutput { reset: true, .. }),
            "second client's snapshot leaked to first"
        );
    }
    link.command_blocking(Command::CaptureTerminalSelection {
        session_id,
        terminal_id,
        selection: Some(tcode_protocol::TerminalSelection {
            line_start: 2,
            line_end: 3,
            text: "text selected in the remote grid".into(),
        }),
    })
    .unwrap();
    next(&events, |event| {
        matches!(event, ServerEvent::SessionStatusReplaced(status)
            if status.terminal_contexts.iter().any(|context|
                context.text == "text selected in the remote grid"
                    && context.line_start == 2 && context.line_end == 3))
    });
    link.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}

#[test]
fn preview_mux_request_reply_first_responder_and_no_subscriber_timeout() {
    let (host, mux, link, session_id) = fixture();
    let second = linked(&mux);
    for client in [&link, &second] {
        client
            .subscribe(Subscription {
                topic: Topic::Preview {
                    session_id: session_id.clone(),
                },
                after: None,
            })
            .unwrap();
        client.command_blocking(Command::OpenLatestSession).unwrap(); // subscription barrier
    }
    let events = link.events();
    let other_events = second.events();
    let (requests, receiver) = async_channel::unbounded();
    smol::block_on(
        host.update_state_for_test(move |state, cx| {
            state.pump_preview_requests(Some(receiver), cx)
        }),
    )
    .unwrap();
    let (reply, answer) = async_channel::bounded(1);
    requests
        .send_blocking(preview_mcp::BrokerRequest {
            session_id: session_id.clone(),
            op: preview_mcp::PreviewOp::Navigate {
                url: "http://localhost:5173".into(),
            },
            reply,
        })
        .unwrap();
    let ServerEvent::PreviewRequest {
        request_id,
        session_id: delivered,
        request,
    } = next(&events, |event| {
        matches!(event, ServerEvent::PreviewRequest { .. })
    })
    else {
        unreachable!()
    };
    assert_eq!(delivered, session_id);
    assert_eq!(
        request,
        tcode_protocol::PreviewRequest::Navigate {
            url: "http://localhost:5173".into()
        }
    );
    next(
        &other_events,
        |event| matches!(event, ServerEvent::PreviewRequest { request_id: id, .. } if *id == request_id),
    );
    let json = serde_json::json!({"url":"http://host.lan:5173"});
    link.command_blocking(Command::PreviewReply {
        request_id,
        response: Ok(tcode_protocol::PreviewResponse::Json(json.clone())),
    })
    .unwrap();
    assert_eq!(
        answer.recv_blocking().unwrap(),
        Ok(preview_mcp::PreviewReply::Json(json))
    );
    second
        .command_blocking(Command::PreviewReply {
            request_id,
            response: Err("late response".into()),
        })
        .unwrap();
    assert!(answer.try_recv().is_err());
    let (reply, answer) = async_channel::bounded(1);
    smol::block_on(host.update_state_for_test(move |state, cx| {
        state.route_preview_with_timeout(
            preview_mcp::BrokerRequest {
                session_id: "unviewed-session".into(),
                op: preview_mcp::PreviewOp::Status,
                reply,
            },
            Duration::from_millis(30),
            cx,
        )
    }))
    .unwrap();
    assert!(
        answer
            .recv_blocking()
            .unwrap()
            .unwrap_err()
            .contains("timed out")
    );
    assert!(events.try_recv().is_err());
    link.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}

#[test]
fn attachments_mux_uses_host_session_directory_and_returns_identical_bytes() {
    let (_host, _mux, link, _session_id) = fixture();
    let events = link.events();
    let ServerEvent::SessionStatusReplaced(status) = next(&events, |event| {
        matches!(event, ServerEvent::SessionStatusReplaced(_))
    }) else {
        unreachable!()
    };
    let dir = status.attachments_dir;
    let bytes = vec![0x89, b'P', b'N', b'G', 0, 0xff, 0x80];
    let QueryResponse::SavedAttachment(path) = smol::block_on(link.query(Query::SaveAttachment {
        dir: dir.clone(),
        bytes: bytes.clone(),
        ext: "png".into(),
    }))
    .unwrap() else {
        panic!("missing saved path");
    };
    assert_eq!(path.parent(), Some(dir.as_path()));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        smol::block_on(link.query(Query::ReadFileBytes { path })).unwrap(),
        QueryResponse::FileBytes(bytes)
    );
    link.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}
