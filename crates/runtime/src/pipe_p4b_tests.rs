//! Integration coverage through serialized links and the production fan-out mux.
use super::*;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tcode_protocol::{
    ExternalImportState, ExternalImportStatus, ExternalThread, SessionSearchHit, SourceTool,
};
use tcode_remote::HostMux;

pub(super) fn linked(mux: &HostMux) -> HostLink {
    let connection = mux.attach();
    let link = HostLink::new(connection.to_host, connection.from_host);
    let pump = link.clone();
    smol::spawn(async move { pump.pump().await }).detach();
    link
}

pub(super) fn fixture() -> (SpawnedHost, HostMux, HostLink, String) {
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

pub(super) fn next(
    events: &async_channel::Receiver<EventEnvelope>,
    predicate: impl Fn(&ServerEvent) -> bool,
) -> ServerEvent {
    super::tests::next_event(events, |event| predicate(&event.event)).event
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

/// A host with a project but no session, plus the temp root the store owns.
/// The root is deliberately distinct per test so a sentinel found by search
/// can only have come from this host's own store.
fn project_fixture() -> (SpawnedHost, HostMux, HostLink, PathBuf, String) {
    let root = std::env::temp_dir().join(format!("tcode-p4b-{}", uuid::Uuid::new_v4()));
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let host = spawn_host(
        SessionStore::open_at(root.clone()).unwrap(),
        HostServices::default(),
    )
    .unwrap();
    let mux = HostMux::new(host.to_host.clone(), host.from_host.clone());
    let link = linked(&mux);
    let CommandResponse::ProjectId(Some(project_id)) = link
        .command_blocking(Command::CreateProject { root: project })
        .unwrap()
    else {
        panic!("no project");
    };
    (host, mux, link, root, project_id)
}

/// Write a minimal Claude Code transcript and the thread record pointing at it.
fn claude_thread(dir: &Path, id: &str, text: &str) -> ExternalThread {
    let file = dir.join(format!("{id}.jsonl"));
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        &file,
        format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": text},
                "timestamp": "2026-01-02T03:04:05.006Z",
                "cwd": "/synthetic",
                "sessionId": id,
            }),
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "acknowledged"}]},
                "timestamp": "2026-01-02T03:04:06.006Z",
                "cwd": "/synthetic",
                "sessionId": id,
            }),
        ),
    )
    .unwrap();
    ExternalThread {
        source: SourceTool::ClaudeCode,
        file,
        external_id: format!("claude:{id}"),
        title_hint: None,
        last_active_ms: 1_767_322_800_000,
    }
}

fn import_status(event: &ServerEvent) -> Option<&ExternalImportStatus> {
    match event {
        ServerEvent::ExternalImportStatusReplaced { status, .. } => status.as_ref(),
        _ => None,
    }
}

#[test]
fn empty_import_completes_without_a_client_and_stays_readable_from_a_late_snapshot() {
    let (_host, mux, link, _root, project_id) = project_fixture();
    let events = link.events();
    link.subscribe(Subscription {
        after: None,
        topic: Topic::ExternalImport {
            project_id: project_id.clone(),
        },
    })
    .unwrap();
    // Before any run the retained status is explicitly absent.
    assert!(
        import_status(&next(&events, |event| matches!(
            event,
            ServerEvent::ExternalImportStatusReplaced { .. }
        )))
        .is_none()
    );

    assert_eq!(
        link.command_blocking(Command::StartExternalImport {
            project_id: project_id.clone(),
            threads: Vec::new(),
        })
        .unwrap(),
        CommandResponse::ExternalImportStarted(true)
    );
    let finished = next(&events, |event| {
        matches!(
            import_status(event).map(|status| &status.state),
            Some(ExternalImportState::Finished { .. })
        )
    });
    let run_id = import_status(&finished).unwrap().run_id;
    assert_eq!(
        import_status(&finished).unwrap().state,
        ExternalImportState::Finished {
            imported: 0,
            skipped: 0,
        }
    );

    // A client that attaches only after that instantaneous completion still
    // recovers the outcome, because the snapshot retains the latest run.
    let late = linked(&mux);
    let late_events = late.events();
    late.subscribe(Subscription {
        after: None,
        topic: Topic::ExternalImport {
            project_id: project_id.clone(),
        },
    })
    .unwrap();
    let recovered = next(&late_events, |event| {
        matches!(event, ServerEvent::ExternalImportStatusReplaced { .. })
    });
    assert_eq!(import_status(&recovered).unwrap().run_id, run_id);
    assert_eq!(
        import_status(&recovered).unwrap().state,
        ExternalImportState::Finished {
            imported: 0,
            skipped: 0,
        }
    );

    // A finished run does not block the next one; only a live one does.
    assert_eq!(
        link.command_blocking(Command::StartExternalImport {
            project_id,
            threads: Vec::new(),
        })
        .unwrap(),
        CommandResponse::ExternalImportStarted(true)
    );
    assert_eq!(
        link.command_blocking(Command::StartExternalImport {
            project_id: "missing".into(),
            threads: Vec::new(),
        })
        .unwrap(),
        CommandResponse::ExternalImportStarted(false)
    );
    link.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}

#[test]
fn a_second_start_while_a_run_is_live_is_rejected() {
    let (host, _mux, link, _root, project_id) = project_fixture();
    let live = project_id.clone();
    smol::block_on(host.update_state_for_test(move |state, cx| {
        state.replace_external_import_status(
            &live,
            Some(ExternalImportStatus {
                run_id: 7,
                state: ExternalImportState::Progress {
                    done: 1,
                    total: 9,
                    tool: "Claude Code".into(),
                },
            }),
            cx,
        );
    }))
    .unwrap();
    let error = link
        .command_blocking(Command::StartExternalImport {
            project_id,
            threads: Vec::new(),
        })
        .unwrap_err();
    assert_eq!(error.code, "import_in_progress");
    link.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}

#[test]
fn import_finalizes_the_index_before_finished_even_after_the_initiator_disconnects() {
    let (_host, mux, watcher, root, project_id) = project_fixture();
    let history = root.join("history");
    let threads = vec![
        claude_thread(&history, "thread-a", "first imported conversation"),
        claude_thread(&history, "thread-b", "second imported conversation"),
    ];

    let events = watcher.events();
    for topic in [
        Topic::Index,
        Topic::ExternalImport {
            project_id: project_id.clone(),
        },
    ] {
        watcher
            .subscribe(Subscription { after: None, topic })
            .unwrap();
    }
    next(&events, |event| {
        matches!(event, ServerEvent::ExternalImportStatusReplaced { .. })
    });

    // The initiator is a raw mux connection so the test can genuinely drop it
    // mid-run; completion must not depend on it still being attached.
    let initiator = mux.attach();
    initiator
        .to_host
        .send_blocking(
            tcode_protocol::encode_line(&ClientMessage {
                key: None,
                id: 1,
                payload: ClientPayload::Command(Command::StartExternalImport {
                    project_id: project_id.clone(),
                    threads,
                }),
            })
            .unwrap(),
        )
        .unwrap();
    loop {
        let line = initiator.from_host.recv_blocking().unwrap();
        if let HostMessage::Ack { id: 1, result } = tcode_protocol::decode_host_line(&line).unwrap()
        {
            assert_eq!(
                result.unwrap(),
                CommandResponse::ExternalImportStarted(true)
            );
            break;
        }
    }
    drop(initiator);

    // Scan in arrival order: the finalized index must precede `Finished`.
    let mut index_ready = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "import never finished"
        );
        let Ok(envelope) = events.recv_blocking() else {
            panic!("event stream closed before the import finished")
        };
        match &envelope.event {
            ServerEvent::IndexSnapshot(snapshot) => {
                index_ready = snapshot
                    .sessions
                    .iter()
                    .filter(|meta| meta.project_id.as_deref() == Some(project_id.as_str()))
                    .count()
                    == 2;
            }
            ServerEvent::ExternalImportStatusReplaced {
                status: Some(status),
                ..
            } => {
                if let ExternalImportState::Finished { imported, skipped } = status.state {
                    assert_eq!((imported, skipped), (2, 0));
                    assert!(
                        index_ready,
                        "Finished reached a client before the finalized index"
                    );
                    break;
                }
            }
            _ => {}
        }
    }
    watcher
        .command_blocking(Command::ShutdownAllAndFlush)
        .unwrap();
}

#[test]
fn content_search_is_host_owned_and_survives_appends_limits_and_blank_queries() {
    let (_host, _mux, link, root, project_id) = project_fixture();
    let history = root.join("history");
    // The sentinel exists only under this host's temp root; a client-local
    // store rooted at the process data dir could never produce this hit.
    let threads = vec![claude_thread(
        &history,
        "searchable",
        "please review zqxsentinel handling",
    )];
    link.subscribe(Subscription {
        after: None,
        topic: Topic::ExternalImport {
            project_id: project_id.clone(),
        },
    })
    .unwrap();
    let events = link.events();
    link.command_blocking(Command::StartExternalImport {
        project_id,
        threads,
    })
    .unwrap();
    next(&events, |event| {
        matches!(
            import_status(event).map(|status| &status.state),
            Some(ExternalImportState::Finished { .. })
        )
    });

    let search = |query: &str, limit: u32| -> Vec<SessionSearchHit> {
        match smol::block_on(link.query(Query::SearchSessionContent {
            query: query.into(),
            limit,
        }))
        .unwrap()
        {
            QueryResponse::SessionContentHits(hits) => hits,
            other => panic!("unexpected search response: {other:?}"),
        }
    };

    let hits = search("zqxsentinel", 50);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].turn, 0);
    assert!(hits[0].snippet.contains("zqxsentinel"));
    let session_id = hits[0].session_id.clone();

    // Whitespace-only and zero-limit requests are answered, not searched.
    assert!(search("   ", 50).is_empty());
    assert!(search("zqxsentinel", 0).is_empty());

    // Appending to the same log must invalidate the cached parse. Sixty new
    // matches also prove the host clamps an over-large client limit to 50.
    let store = SessionStore::open_at(root.clone()).unwrap();
    for index in 0..60 {
        store
            .append_event(
                &session_id,
                100 + index,
                &agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: format!("appended-{index}"),
                    parent_item_id: None,
                    content: agent::ItemContent::AssistantMessage {
                        text: format!("zqxsentinel appended {index}"),
                    },
                }),
            )
            .unwrap();
    }
    let hits = search("zqxsentinel", 1_000);
    assert_eq!(hits.len(), 50);
    assert!(
        hits.iter()
            .any(|hit| hit.snippet.contains("zqxsentinel appended 0"))
    );
    assert_eq!(search("zqxsentinel", 3).len(), 3);
    link.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}
