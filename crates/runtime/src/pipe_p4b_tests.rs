//! Integration coverage through serialized links and the production fan-out mux.
use super::*;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tcode_protocol::{
    ExternalImportState, ExternalImportStatus, ExternalThread, SessionSearchHit, SourceTool,
};
use tcode_traverse::HostMux;

pub(super) fn linked(mux: &HostMux) -> HostLink {
    let connection = mux.attach(
        tcode_protocol::Principal::Full,
        tcode_client::attachment::AttachmentPolicy::network(),
    );
    let link = HostLink::new(connection.to_host, connection.from_host);
    let pump = link.clone();
    smol::spawn(async move { pump.pump().await }).detach();
    link
}

// The host continues publishing while requests wait; retain events for assertions.
pub(crate) fn observed_events(link: &HostLink) -> async_channel::Receiver<EventEnvelope> {
    let events = link.events();
    let (observed, received) = async_channel::unbounded();
    smol::spawn(async move {
        smol::future::race(
            async {
                while let Ok(event) = events.recv().await {
                    if observed.send(event).await.is_err() {
                        break;
                    }
                }
            },
            observed.closed(),
        )
        .await;
    })
    .detach();
    received
}

pub(super) fn fixture() -> (SpawnedHost, HostMux, HostLink, String) {
    let (host, mux, link, session_id, _) = fixture_in_project();
    (host, mux, link, session_id)
}

/// [`fixture`], with the thread's directory.
fn fixture_in_project() -> (SpawnedHost, HostMux, HostLink, String, PathBuf) {
    let root = std::env::temp_dir().join(format!("tcode-p4b-{}", uuid::Uuid::new_v4()));
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let host = spawn_host(
        SessionStore::open_at(root).unwrap(),
        HostServices::default(),
    )
    .unwrap();
    let mux = HostMux::new(host.attachments.clone(), host.from_host.clone());
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
            cwd: project.clone(),
        })
        .unwrap()
    else {
        panic!("no session");
    };
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::SessionStatus {
            session_id: session_id.clone(),
        },
        after: None,
    })
    .unwrap();
    (host, mux, link, session_id, project)
}

pub(super) fn next(
    events: &async_channel::Receiver<EventEnvelope>,
    predicate: impl Fn(&ServerEvent) -> bool,
) -> ServerEvent {
    super::tests::next_event(events, |event| predicate(&event.event)).event
}

/// Subscribe `link` to its preview channel as a client that renders pages.
fn preview_client(link: &HostLink, name: &str) -> tcode_protocol::PreviewClient {
    let client = tcode_protocol::PreviewClient {
        id: format!("{name}-id"),
        name: name.into(),
    };
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Preview {
            client: client.clone(),
        },
        after: None,
    })
    .unwrap();
    link.command_blocking(Command::OpenLatestSession).unwrap(); // subscription barrier
    client
}

/// An agent's preview call, as the preview MCP broker hands it to the host.
fn agent_call(
    host: &SpawnedHost,
    session_id: &str,
    op: preview_mcp::PreviewOp,
) -> async_channel::Receiver<Result<preview_mcp::PreviewReply, String>> {
    let (reply, answer) = async_channel::bounded(1);
    let session_id = session_id.to_owned();
    smol::block_on(host.update_state_for_test(move |state, cx| {
        state.route_preview(
            preview_mcp::BrokerRequest {
                session_id,
                op,
                reply,
            },
            cx,
        )
    }))
    .unwrap();
    answer
}

fn show_here(
    link: &HostLink,
    session_id: &str,
    client: &tcode_protocol::PreviewClient,
    take_over: bool,
) {
    link.command_blocking(Command::ShowPreviewHere {
        session_id: session_id.into(),
        client_id: client.id.clone(),
        take_over,
    })
    .unwrap();
}

fn rendering(event: &ServerEvent) -> Option<Vec<String>> {
    match event {
        ServerEvent::PreviewRenderingReplaced(sessions) => Some(
            sessions
                .iter()
                .map(|session| session.session_id.clone())
                .collect(),
        ),
        _ => None,
    }
}

fn watch_session(link: &HostLink, session_id: &str) {
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::PreviewSession {
            session_id: session_id.into(),
        },
        after: None,
    })
    .unwrap();
}

fn session_replaced(event: &ServerEvent) -> Option<&tcode_protocol::PreviewSession> {
    match event {
        ServerEvent::PreviewSessionReplaced(session) => Some(session),
        _ => None,
    }
}

fn answer(link: &HostLink, request_id: u64, json: serde_json::Value) {
    link.command_blocking(Command::PreviewReply {
        request_id,
        response: Ok(tcode_protocol::PreviewResponse::Json(json)),
    })
    .unwrap();
}

fn request_id(event: ServerEvent) -> u64 {
    match event {
        ServerEvent::PreviewRequest { request_id, .. } => request_id,
        other => panic!("expected a preview request, got {other:?}"),
    }
}

#[test]
fn an_agent_preview_request_reaches_only_the_client_rendering_its_thread() {
    let (host, mux, desk, session_id) = fixture();
    let refused = agent_call(&host, &session_id, preview_mcp::PreviewOp::Status);
    assert!(
        refused
            .recv_blocking()
            .unwrap()
            .unwrap_err()
            .contains("no connected client has a preview browser")
    );
    let laptop = linked(&mux);
    let desk_events = observed_events(&desk);
    let laptop_events = observed_events(&laptop);
    let desk_client = preview_client(&desk, "Desk");
    preview_client(&laptop, "Laptop");
    show_here(&desk, &session_id, &desk_client, false);

    let answered = agent_call(
        &host,
        &session_id,
        preview_mcp::PreviewOp::Navigate {
            url: "http://localhost:5173".into(),
        },
    );
    let ServerEvent::PreviewRequest {
        request_id,
        session_id: delivered,
        request,
    } = next(&desk_events, |event| {
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
    // Subscribing now answers after anything the host sent the laptop before.
    watch_session(&laptop, &session_id);
    let seen = next(&laptop_events, |event| {
        matches!(
            event,
            ServerEvent::PreviewRequest { .. } | ServerEvent::PreviewSessionReplaced(_)
        )
    });
    assert_eq!(
        session_replaced(&seen).and_then(|session| session.renderer.clone()),
        Some(desk_client.clone()),
        "only the rendering client receives the request"
    );
    let json = serde_json::json!({"url":"http://host.lan:5173"});
    answer(&desk, request_id, json.clone());
    assert_eq!(
        answered.recv_blocking().unwrap(),
        Ok(preview_mcp::PreviewReply::Json(json))
    );

    desk.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}

#[test]
fn show_here_moves_rendering_and_the_page_with_it() {
    let (host, mux, desk, session_id) = fixture();
    let laptop = linked(&mux);
    let desk_events = observed_events(&desk);
    let laptop_events = observed_events(&laptop);
    let desk_client = preview_client(&desk, "Desk");
    let laptop_client = preview_client(&laptop, "Laptop");
    show_here(&desk, &session_id, &desk_client, false);
    let tab = tcode_protocol::PreviewTab {
        url: Some("http://localhost:5173/settings".into()),
        canvas: Some((390, 844)),
    };
    desk.command_blocking(Command::ReportPreviewTabs {
        session_id: session_id.clone(),
        client_id: desk_client.id.clone(),
        tabs: vec![tab.clone()],
    })
    .unwrap();
    watch_session(&laptop, &session_id);
    let watched = next(&laptop_events, |event| session_replaced(event).is_some());
    assert_eq!(
        session_replaced(&watched).unwrap().renderer,
        Some(desk_client.clone())
    );

    // Showing the preview where another client renders it does not take
    // it, and only the rendering client's pages are the thread's.
    show_here(&laptop, &session_id, &laptop_client, false);
    laptop
        .command_blocking(Command::ReportPreviewTabs {
            session_id: session_id.clone(),
            client_id: laptop_client.id.clone(),
            tabs: Vec::new(),
        })
        .unwrap();
    let tab = tcode_protocol::PreviewTab {
        url: Some("http://localhost:5173/profile".into()),
        ..tab
    };
    desk.command_blocking(Command::ReportPreviewTabs {
        session_id: session_id.clone(),
        client_id: desk_client.id.clone(),
        tabs: vec![tab.clone()],
    })
    .unwrap();
    let changed = next(&laptop_events, |event| session_replaced(event).is_some());
    let changed = session_replaced(&changed).unwrap();
    assert_eq!(changed.renderer, Some(desk_client.clone()));
    assert_eq!(changed.tabs, std::slice::from_ref(&tab));

    show_here(&laptop, &session_id, &laptop_client, true);
    let ServerEvent::PreviewRenderingReplaced(taken) = next(&laptop_events, |event| {
        rendering(event).is_some_and(|sessions| !sessions.is_empty())
    }) else {
        unreachable!()
    };
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].session_id, session_id);
    assert_eq!(taken[0].tabs, [tab], "the page reopens where it was");
    assert_eq!(taken[0].renderer, Some(laptop_client.clone()));
    next(&desk_events, |event| {
        rendering(event).is_some_and(|sessions| sessions.is_empty())
    });

    let answered = agent_call(&host, &session_id, preview_mcp::PreviewOp::Status);
    let request_id = request_id(next(&laptop_events, |event| {
        matches!(event, ServerEvent::PreviewRequest { .. })
    }));
    answer(&laptop, request_id, serde_json::json!({}));
    assert_eq!(
        answered.recv_blocking().unwrap().unwrap(),
        preview_mcp::PreviewReply::Json(serde_json::json!({
            "userActedSinceSnapshot": false,
            "renderedOn": "Laptop",
        }))
    );
    // The desk let go before the request went out: it never sees it.
    watch_session(&desk, &session_id);
    let seen = next(&desk_events, |event| {
        matches!(
            event,
            ServerEvent::PreviewRequest { .. } | ServerEvent::PreviewSessionReplaced(_)
        )
    });
    assert!(session_replaced(&seen).is_some());
    desk.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}

#[test]
fn a_rendering_client_that_goes_away_releases_its_threads() {
    let (host, mux, desk, session_id) = fixture();
    let desk_events = observed_events(&desk);
    let desk_client = preview_client(&desk, "Desk");
    // The phone is a raw attachment so the test can close its link, as a
    // phone does when it goes to the background.
    let connection = mux.attach(
        tcode_protocol::Principal::Full,
        tcode_client::attachment::AttachmentPolicy::network(),
    );
    let phone_link = connection.to_host.clone();
    let phone = HostLink::new(connection.to_host, connection.from_host);
    let pump = phone.clone();
    smol::spawn(async move { pump.pump().await }).detach();
    let phone_events = observed_events(&phone);
    let phone_client = preview_client(&phone, "Phone");
    show_here(&phone, &session_id, &phone_client, true);
    let tab = tcode_protocol::PreviewTab {
        url: Some("http://localhost:3000/".into()),
        canvas: None,
    };
    phone
        .command_blocking(Command::ReportPreviewTabs {
            session_id: session_id.clone(),
            client_id: phone_client.id.clone(),
            tabs: vec![tab.clone()],
        })
        .unwrap();
    watch_session(&desk, &session_id);

    let orphaned = agent_call(&host, &session_id, preview_mcp::PreviewOp::Status);
    next(&phone_events, |event| {
        matches!(event, ServerEvent::PreviewRequest { .. })
    });
    phone_link.close();
    let error = orphaned.recv_blocking().unwrap().unwrap_err();
    assert!(
        error.contains("Phone") && error.contains("went away"),
        "{error}"
    );
    next(&desk_events, |event| {
        session_replaced(event).is_some_and(|session| session.renderer.is_none())
    });

    // The next call opens the page on a client still here.
    let answered = agent_call(&host, &session_id, preview_mcp::PreviewOp::Status);
    let ServerEvent::PreviewRenderingReplaced(taken) = next(&desk_events, |event| {
        rendering(event).is_some_and(|sessions| !sessions.is_empty())
    }) else {
        unreachable!()
    };
    assert_eq!(taken[0].tabs, [tab]);
    assert_eq!(taken[0].renderer, Some(desk_client));
    let request_id = request_id(next(&desk_events, |event| {
        matches!(event, ServerEvent::PreviewRequest { .. })
    }));
    answer(&desk, request_id, serde_json::json!({}));
    assert!(answered.recv_blocking().unwrap().is_ok());
    desk.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}

#[test]
fn user_input_on_the_rendering_page_reaches_the_agent_until_it_snapshots() {
    let (host, mux, desk, session_id) = fixture();
    let laptop = linked(&mux);
    let desk_events = observed_events(&desk);
    let laptop_events = observed_events(&laptop);
    let desk_client = preview_client(&desk, "Desk");
    let laptop_client = preview_client(&laptop, "Laptop");
    show_here(&desk, &session_id, &desk_client, false);
    watch_session(&laptop, &session_id);
    let status = |expected: bool| {
        let answered = agent_call(&host, &session_id, preview_mcp::PreviewOp::Status);
        let request_id = request_id(next(&desk_events, |event| {
            matches!(event, ServerEvent::PreviewRequest { .. })
        }));
        answer(
            &desk,
            request_id,
            serde_json::json!({"url": "http://localhost:5173/"}),
        );
        let Ok(preview_mcp::PreviewReply::Json(value)) = answered.recv_blocking().unwrap() else {
            panic!("status failed");
        };
        assert_eq!(value["userActedSinceSnapshot"], expected, "{value}");
    };

    // Input on a page the laptop does not render is not the thread's.
    laptop
        .command_blocking(Command::ReportPreviewInput {
            session_id: session_id.clone(),
            client_id: laptop_client.id.clone(),
        })
        .unwrap();
    status(false);
    desk.command_blocking(Command::ReportPreviewInput {
        session_id: session_id.clone(),
        client_id: desk_client.id.clone(),
    })
    .unwrap();
    next(&laptop_events, |event| {
        session_replaced(event).is_some_and(|session| session.user_acted_since_snapshot)
    });
    status(true);

    let snapshot = agent_call(&host, &session_id, preview_mcp::PreviewOp::Snapshot);
    let request_id = request_id(next(&desk_events, |event| {
        matches!(event, ServerEvent::PreviewRequest { .. })
    }));
    answer(&desk, request_id, serde_json::json!({"elements": []}));
    assert!(snapshot.recv_blocking().unwrap().is_ok());
    next(&laptop_events, |event| {
        session_replaced(event).is_some_and(|session| !session.user_acted_since_snapshot)
    });
    status(false);
    desk.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}

#[test]
fn runtime_read_budget_comes_from_the_attachment() {
    let root = std::env::temp_dir().join(format!("tcode-policy-{}", uuid::Uuid::new_v4()));
    let store = SessionStore::open_at(root.clone()).unwrap();
    let path = root.join("original.bin");
    let bytes = b"original";
    std::fs::write(&path, bytes).unwrap();
    let host = spawn_host(store, HostServices::default()).unwrap();
    let line = tcode_protocol::encode_line(&ClientMessage {
        id: 1,
        key: None,
        principal: None,
        payload: ClientPayload::Query(Query::ReadFileBytes {
            path,
            max_bytes: Some(u64::MAX),
        }),
    })
    .unwrap();
    let network = AttachmentPolicy {
        read_budget: Some(2),
        ..AttachmentPolicy::network()
    };
    for (policy, expected) in [
        (
            AttachmentPolicy::LOCAL,
            Ok(QueryResponse::FileBytes(bytes.to_vec())),
        ),
        (network, Err("file_too_large".to_string())),
    ] {
        let attachment = AttachmentSender::new(host.attachments.clone(), policy);
        attachment.try_send(line.clone()).unwrap();
        let result = loop {
            if let HostMessage::QueryResult { id: 1, result } =
                tcode_protocol::decode_host_line(&host.from_host.recv_blocking().unwrap()).unwrap()
            {
                break result;
            }
        };
        assert_eq!(result.map_err(|error| error.code), expected);
    }
    host.shutdown_blocking().unwrap();
    std::fs::remove_dir_all(root).unwrap();
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
    let mux = HostMux::new(host.attachments.clone(), host.from_host.clone());
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
fn import_status_survives_clients_and_gates_new_runs_by_lifecycle() {
    let (host, mux, link, _root, project_id) = project_fixture();
    let events = observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
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
    assert_eq!(
        link.command_blocking(Command::StartExternalImport {
            project_id: project_id.clone(),
            threads: Vec::new(),
        })
        .unwrap_err()
        .code,
        "import_in_progress"
    );
    let reset = project_id.clone();
    smol::block_on(host.update_state_for_test(move |state, cx| {
        state.replace_external_import_status(&reset, None, cx);
    }))
    .unwrap();

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
    let late_events = observed_events(&late);
    late.subscribe(Subscription {
        known_index: None,
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
fn import_finalizes_the_index_before_finished_even_after_the_initiator_disconnects() {
    let (host, mux, watcher, root, project_id) = project_fixture();
    let mut owner = tcode_core::project::SessionMeta::new(
        agent::ProviderKind::ClaudeCode,
        root.join("project"),
        None,
    );
    owner.id = "archived-owner".into();
    owner.archived_at = Some(1);
    owner.worktree = Some(tcode_core::project::WorktreeInfo {
        root_project_path: root.join("repo"),
        base: "main".into(),
        branch: "tcode/owner".into(),
    });
    SessionStore::open_at(root.clone())
        .unwrap()
        .upsert_meta(&owner)
        .unwrap();
    smol::block_on(host.update_state_for_test(move |state, _cx| {
        state.sessions.push(owner);
    }))
    .unwrap();
    let QueryResponse::ArchivedSessions(before) =
        smol::block_on(watcher.query(Query::ArchivedSessions)).unwrap()
    else {
        panic!("expected archived sessions");
    };
    assert!(!before.worktree_shared.contains("archived-owner"));
    let history = root.join("history");
    let threads = vec![
        claude_thread(&history, "thread-a", "first imported conversation"),
        claude_thread(&history, "thread-b", "second imported conversation"),
    ];

    let events = observed_events(&watcher);
    for topic in [
        Topic::Index,
        Topic::ExternalImport {
            project_id: project_id.clone(),
        },
    ] {
        watcher
            .subscribe(Subscription {
                known_index: None,
                after: None,
                topic,
            })
            .unwrap();
    }
    next(&events, |event| {
        matches!(event, ServerEvent::ExternalImportStatusReplaced { .. })
    });

    // The initiator is a raw mux connection so the test can genuinely drop it
    // mid-run; completion must not depend on it still being attached.
    let initiator = mux.attach(
        tcode_protocol::Principal::Full,
        tcode_client::attachment::AttachmentPolicy::network(),
    );
    initiator
        .to_host
        .send_blocking(
            tcode_protocol::encode_line(&ClientMessage {
                principal: None,
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
    let mut imported_ids = std::collections::HashSet::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "import never finished"
        );
        let envelope = super::tests::next_event(&events, |_| true);
        match &envelope.event {
            ServerEvent::IndexUpsertSession(meta)
                if meta.project_id.as_deref() == Some(project_id.as_str()) =>
            {
                imported_ids.insert(meta.id.clone());
            }
            ServerEvent::ExternalImportStatusReplaced {
                status: Some(status),
                ..
            } => {
                if let ExternalImportState::Finished { imported, skipped } = status.state {
                    assert_eq!((imported, skipped), (2, 0));
                    assert_eq!(
                        imported_ids.len(),
                        2,
                        "Finished reached a client before the finalized index"
                    );
                    break;
                }
            }
            _ => {}
        }
    }
    let QueryResponse::ArchivedSessions(after) =
        smol::block_on(watcher.query(Query::ArchivedSessions)).unwrap()
    else {
        panic!("expected archived sessions");
    };
    assert!(after.revision > before.revision);
    assert!(after.worktree_shared.contains("archived-owner"));
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
        known_index: None,
        after: None,
        topic: Topic::ExternalImport {
            project_id: project_id.clone(),
        },
    })
    .unwrap();
    let events = observed_events(&link);
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
                        summarized: false,
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

/// A dev server the user started in a thread's directory: a process other
/// than the host, listening on loopback until its stdin closes.
#[test]
#[ignore = "the dev server of a_remote_client_sees_the_dev_servers_the_host_machine_runs, which starts it"]
fn dev_server_process() {
    use std::io::Write as _;
    // A port of its own choosing, as dev servers have: below the range the
    // system hands out to whoever asks for any.
    let first = 20_000 + (std::process::id() % 10_000) as u16;
    let listener = (first..30_000)
        .chain(20_000..first)
        .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
        .unwrap();
    println!("listening:{}", listener.local_addr().unwrap().port());
    std::io::stdout().flush().unwrap();
    let _ = std::io::read_to_string(std::io::stdin());
}

#[test]
fn a_remote_client_sees_the_dev_servers_the_host_machine_runs() {
    use std::io::BufRead as _;
    let (_host, mux, desk, session_id, project) = fixture_in_project();
    let phone = linked(&mux);
    let events = observed_events(&phone);
    let executable = std::env::current_exe().unwrap();
    let mut server = tcode_services::process::command(&executable)
        .args([
            "--exact",
            "pipe::p4b_tests::dev_server_process",
            "--ignored",
            "--nocapture",
        ])
        .current_dir(&project)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let port: u16 = std::io::BufReader::new(server.stdout.take().unwrap())
        .lines()
        .find_map(|line| line.unwrap().strip_prefix("listening:")?.parse().ok())
        .unwrap();
    phone
        .subscribe(Subscription {
            known_index: None,
            topic: Topic::DevServers {
                session_id: session_id.clone(),
            },
            after: None,
        })
        .unwrap();
    let listed = |event: &ServerEvent| match event {
        ServerEvent::DevServersReplaced(servers) => Some(servers.clone()),
        _ => None,
    };
    let servers = listed(&next(&events, |event| {
        listed(event).is_some_and(|servers| servers.iter().any(|server| server.port == port))
    }))
    .unwrap();
    let name = if cfg!(windows) {
        executable.file_stem()
    } else {
        executable.file_name()
    };
    let position = servers.iter().position(|server| server.port == port);
    if !cfg!(windows) {
        assert_eq!(
            position,
            Some(0),
            "the thread's own dev server comes first: {servers:?}"
        );
    }
    assert_eq!(
        servers[position.unwrap()],
        tcode_protocol::DevServer {
            port,
            pid: server.id(),
            process: name.unwrap().to_string_lossy().into_owned(),
            // Windows tells no other process's working directory cheaply.
            cwd: (!cfg!(windows)).then(String::new),
        }
    );

    drop(server.stdin.take());
    server.wait().unwrap();
    desk.command_blocking(Command::ShutdownAllAndFlush).unwrap();
}
