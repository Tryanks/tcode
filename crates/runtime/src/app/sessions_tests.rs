use crate::app::test_support::*;
use crate::app::{Runtime, ScriptedProvider, scripted_provider};
use agent::{AgentError, AgentEvent, ProviderKind, SessionCommand};
use std::path::PathBuf;
use std::sync::Arc;
use tcode_core::project::{Project, SessionMeta};
use tcode_protocol::{
    ClientPayload, Command, CommandResponse, EventEnvelope, HostMessage, ProtocolError,
    RuntimeError, RuntimeNotification, ServerEvent, Subscription, Topic,
};

use super::{ProviderLaunchFuture, ProviderLauncher};

/// Sends one command, runs the host until it answers, and returns the answer
/// with everything the host sent on the way.
fn acked(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    command: Command,
) -> (Result<CommandResponse, ProtocolError>, Vec<HostMessage>) {
    state.dispatch_command(cx, id, command);
    cx.run_until_sent(|message| match message {
        HostMessage::Ack { id: acked, result } if *acked == id => Some(result.clone()),
        _ => None,
    })
}

#[test]
fn set_project_icon_saves_replaces_and_resets_the_managed_override() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-project-icon");
    let project = Project::from_root(store.root().join("project"));
    std::fs::create_dir_all(&project.root).unwrap();
    store.upsert_project(&project).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.deliver(
        cx,
        100,
        ClientPayload::Subscribe(Subscription {
            known_index: None,
            topic: Topic::Index,
            after: None,
        }),
    );
    cx.run_until_parked();
    cx.drain_outgoing();
    // The picker sends the host's own thumbnail of the chosen image.
    let source = project.root.join("logo.png");
    std::fs::write(
        &source,
        include_bytes!("../../../../assets/icons/app/tcode.png"),
    )
    .unwrap();
    let png = tcode_services::project_icons::thumbnail(&source).unwrap();
    let icons = store.root().join("project-icons");
    let stored = || store.read_file().unwrap().projects[0].icon_path.clone();
    let upserts = |sent: &[HostMessage]| -> Vec<Option<PathBuf>> {
        sent.iter()
            .filter_map(|message| match message {
                HostMessage::Event(EventEnvelope {
                    topic: Topic::Index,
                    event: ServerEvent::IndexUpsertProject(upserted),
                    ..
                }) if upserted.id == project.id => Some(upserted.icon_path.clone()),
                _ => None,
            })
            .collect()
    };
    let set = |png: Option<Vec<u8>>| Command::SetProjectIcon {
        project_id: project.id.clone(),
        png,
    };

    // A reset that changes nothing still tells clients to reload the default.
    let (result, sent) = acked(&state, cx, 1, set(None));
    assert_eq!(result, Ok(CommandResponse::Unit));
    assert_eq!(upserts(&sent), [None]);
    assert_eq!(stored(), None);

    let mut previous = None::<PathBuf>;
    for id in [2, 3] {
        let (result, sent) = acked(&state, cx, id, set(Some(png.clone())));
        assert_eq!(result, Ok(CommandResponse::Unit));
        let saved = match upserts(&sent).as_slice() {
            [Some(saved)] => saved.clone(),
            other => panic!("one upsert carrying the override, got {other:?}"),
        };
        assert_eq!(saved.parent(), Some(icons.as_path()));
        assert_eq!(std::fs::read(&saved).unwrap(), png);
        assert_eq!(stored().as_ref(), Some(&saved));
        if let Some(previous) = previous.replace(saved.clone()) {
            assert_ne!(previous, saved);
            assert!(!previous.exists(), "a replaced managed copy is deleted");
        }
    }

    let (result, sent) = acked(&state, cx, 4, set(None));
    assert_eq!(result, Ok(CommandResponse::Unit));
    assert_eq!(upserts(&sent), [None]);
    assert_eq!(stored(), None);
    assert!(
        !previous.unwrap().exists(),
        "a reset deletes the managed copy"
    );
    assert!(source.exists(), "the chosen source is never touched");

    for (id, command) in [
        (
            5,
            Command::SetProjectIcon {
                project_id: "missing".into(),
                png: Some(png.clone()),
            },
        ),
        (6, set(Some(b"broken".to_vec()))),
    ] {
        let (result, sent) = acked(&state, cx, id, command);
        assert!(result.is_err(), "{id}: {result:?}");
        assert!(upserts(&sent).is_empty());
    }
    assert_eq!(stored(), None);
    assert_eq!(std::fs::read_dir(&icons).unwrap().count(), 0);
}

/// A scripted provider launched the way `agent::start_session` launches: a
/// missing working directory refuses the spawn. Yields every launch's options.
fn provider_in_real_directories() -> (
    ProviderLauncher,
    ScriptedProvider,
    smol::channel::Receiver<agent::SessionOptions>,
) {
    let scripted = scripted_provider(ProviderKind::Codex);
    let (launched_tx, launched) = smol::channel::unbounded();
    let scripted_launcher = scripted.launcher.clone();
    let launcher = ProviderLauncher(Arc::new(move |kind, options: agent::SessionOptions| {
        if !options.cwd.is_dir() {
            return Box::pin(std::future::ready(Err(AgentError::Spawn(format!(
                "working directory `{}` no longer exists",
                options.cwd.display()
            ))))) as ProviderLaunchFuture;
        }
        launched_tx.try_send(options.clone()).unwrap();
        scripted_launcher.launch(kind, options)
    }));
    (launcher, scripted, launched)
}

/// A project whose directory is gone, holding one thread that has already
/// talked to its provider, opened on a host with `launcher`.
fn thread_in_moved_project(
    store: &TestStore,
    cx: &mut TestAppContext,
    launcher: ProviderLauncher,
) -> (TestEntity, Project) {
    let project = Project::from_root(store.root().join("before"));
    store.upsert_project(&project).unwrap();
    let mut meta = SessionMeta::new(ProviderKind::Codex, project.root.clone(), None);
    meta.id = "thread".into();
    meta.project_id = Some(project.id.clone());
    meta.resume_cursor = Some(agent::ResumeCursor(
        serde_json::json!({"thread_id": "native"}),
    ));
    store.upsert_meta(&meta).unwrap();
    for (ts, event) in [
        (
            1,
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
        ),
        (
            2,
            AgentEvent::ItemCompleted(agent::ThreadItem {
                id: "assistant-1".into(),
                parent_item_id: None,
                content: agent::ItemContent::AssistantMessage {
                    text: "the earlier answer".into(),
                    summarized: false,
                },
            }),
        ),
        (
            3,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: agent::TurnStatus::Completed,
                usage: None,
            },
        ),
    ] {
        store.append_event("thread", ts, &event).unwrap();
    }
    let state = cx.new_entity({
        let mut state = TestClientState::new((**store).clone());
        state.set_provider_launcher(launcher);
        state
    });
    state.update(cx, |state, cx| state.select_session("thread", cx));
    // Opened as a reader opens it, who writes once its history is on screen;
    // the log is read and folded off the host.
    cx.run_until(|state| {
        state
            .resident("thread")
            .is_some_and(|thread| !thread.timeline.entries.is_empty())
    });
    (state, project)
}

fn send(text: &str) -> Command {
    Command::SendTurn {
        session_id: "thread".into(),
        text: text.into(),
        attachment_paths: Vec::new(),
    }
}

/// The self-lock: a thread that could not start because its directory is gone
/// used to refuse the very path change that would let it start.
#[test]
fn a_changed_root_delivers_what_a_missing_directory_left_queued() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-root-queued");
    let (launcher, scripted, launched) = provider_in_real_directories();
    let (state, project) = thread_in_moved_project(&store, cx, launcher);
    let moved_to = store.root().join("after");
    std::fs::create_dir_all(&moved_to).unwrap();

    let (result, sent) = acked(&state, cx, 1, send("do not lose me"));
    assert_eq!(result, Ok(CommandResponse::Unit));
    let mut sent = sent;
    cx.run_until(|state| {
        state
            .resident("thread")
            .is_some_and(|thread| matches!(thread.runtime, Runtime::Idle))
    });
    cx.run_until_parked();
    sent.extend(cx.drain_outgoing());
    assert!(
        sent.iter().any(|message| matches!(
            message,
            HostMessage::Event(EventEnvelope {
                event: ServerEvent::Runtime(RuntimeNotification::Error(
                    RuntimeError::WorkingDirectoryMissing { .. }
                )),
                ..
            })
        )),
        "the failure offers the dialog this path change comes from"
    );
    state.read(|state| {
        let thread = state.resident("thread").unwrap();
        assert_eq!(thread.queue[0].text, "do not lose me");
        let activity = state.session_status_snapshot("thread").unwrap().activity;
        assert!(!activity.working, "a failed start leaves nothing working");
    });

    let change = |root: PathBuf| Command::SetProjectRoot {
        project_id: project.id.clone(),
        root,
        move_files: false,
    };
    let (refused, _) = acked(&state, cx, 2, change(store.root().join("nowhere")));
    assert_eq!(
        refused.map_err(|error| error.code),
        Err("invalid_project_root".to_string())
    );
    state.read(|state| {
        assert_eq!(
            state.resident("thread").unwrap().queue.len(),
            1,
            "a change that did not happen costs nothing"
        );
    });

    let (changed, _) = acked(&state, cx, 3, change(moved_to.clone()));
    assert_eq!(changed, Ok(CommandResponse::Unit));
    let SessionCommand::SendTurn { text, .. } = cx.run_until_command(&scripted.commands) else {
        panic!("the queued message goes out on the new provider");
    };
    let options = launched.try_recv().unwrap();
    assert_eq!(options.cwd, moved_to);
    assert_eq!(
        options.resume,
        Some(agent::ResumeCursor(
            serde_json::json!({"thread_id": "native"})
        )),
        "Codex resumes its native session in the new directory"
    );
    assert_eq!(
        text, "do not lose me",
        "no handoff to a session that has it all"
    );
    assert!(launched.is_empty(), "one start, one delivery");
}

/// A live provider works in the directory; it is waited for, never stopped.
#[test]
fn a_changed_root_waits_for_a_live_provider() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-root-live");
    let (launcher, scripted, launched) = provider_in_real_directories();
    let (state, project) = thread_in_moved_project(&store, cx, launcher);
    let original = store.root().join("before");
    std::fs::create_dir_all(&original).unwrap();
    let moved_to = store.root().join("after");
    std::fs::create_dir_all(&moved_to).unwrap();

    let (result, _) = acked(&state, cx, 1, send("first"));
    assert_eq!(result, Ok(CommandResponse::Unit));
    assert!(matches!(
        cx.run_until_recv(&scripted.commands),
        SessionCommand::SendTurn { .. }
    ));
    assert_eq!(launched.try_recv().unwrap().cwd, original);

    let (refused, _) = acked(
        &state,
        cx,
        2,
        Command::SetProjectRoot {
            project_id: project.id.clone(),
            root: moved_to,
            move_files: false,
        },
    );
    assert_eq!(
        refused.map_err(|error| error.code),
        Err("project_busy".to_string())
    );
    assert!(
        scripted.commands.is_empty(),
        "the refused change stops nothing"
    );
    state.read(|state| {
        assert_eq!(state.find_meta("thread").unwrap().cwd, original);
        assert!(matches!(
            state.resident("thread").unwrap().runtime,
            Runtime::Live(_)
        ));
    });
}

/// A thread the importer cannot read is reported with its file and its reason, not counted
/// among the threads skipped as already imported or empty.
#[test]
fn an_import_that_fails_names_the_thread_and_why() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-import-failed");
    let project = Project::from_root(store.root().join("project"));
    std::fs::create_dir_all(&project.root).unwrap();
    store.upsert_project(&project).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.deliver(
        cx,
        100,
        ClientPayload::Subscribe(Subscription {
            known_index: None,
            topic: Topic::ExternalImport {
                project_id: project.id.clone(),
            },
            after: None,
        }),
    );
    let missing = store.root().join("history/gone.jsonl");
    let thread = tcode_protocol::ExternalThread {
        source: tcode_protocol::SourceTool::ClaudeCode,
        file: missing.clone(),
        external_id: "gone".into(),
        title_hint: None,
        last_active_ms: 1,
        profile_id: None,
    };
    state.dispatch_command(
        cx,
        1,
        Command::StartExternalImport {
            project_id: project.id.clone(),
            threads: vec![thread],
        },
    );
    let (finished, _) = cx.run_until_sent(|message| match message {
        HostMessage::Event(EventEnvelope {
            event:
                ServerEvent::ExternalImportStatusReplaced {
                    status: Some(status),
                    ..
                },
            ..
        }) => match &status.state {
            tcode_protocol::ExternalImportState::Finished {
                imported,
                skipped,
                failed,
            } => Some((*imported, *skipped, failed.clone())),
            _ => None,
        },
        _ => None,
    });
    let (imported, skipped, failed) = finished;
    assert_eq!((imported, skipped), (0, 0));
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].file, missing);
    assert!(!failed[0].error.is_empty());
}

/// A host whose MCP servers did not start says so once, when the first thread that would have
/// had Tcode's tools starts, with the reason.
#[test]
fn a_host_without_its_mcp_servers_says_so_when_the_first_thread_starts() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-mcp-unavailable");
    let projects = ["one", "two"].map(|name| {
        let project = Project::from_root(store.root().join(name));
        std::fs::create_dir_all(&project.root).unwrap();
        store.upsert_project(&project).unwrap();
        project
    });
    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let state = cx.new_entity({
        let mut state = TestClientState::new((*store).clone());
        state.set_provider_launcher(scripted.launcher);
        state
    });
    state.update(cx, |state, _| {
        state.mcp_unavailable(Some("address in use".into()))
    });
    for (id, project) in (1..).zip(&projects) {
        state.dispatch_command(
            cx,
            id,
            Command::StartDraft {
                project_id: project.id.clone(),
                cwd: project.root.clone(),
            },
        );
    }
    let drafts: Vec<String> = state.read(|state| state.residents.live.keys().cloned().collect());
    assert_eq!(drafts.len(), 2);
    for id in &drafts {
        state.update(cx, |state, cx| state.ensure_session_started(id, cx));
    }
    cx.run_until_parked();
    let told: Vec<_> = cx
        .drain_outgoing()
        .into_iter()
        .filter_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                event:
                    ServerEvent::Runtime(RuntimeNotification::Error(RuntimeError::McpUnavailable {
                        error,
                    })),
                ..
            }) => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(told, ["address in use"]);
}

/// Claude Code's history is looked for where each profile's CLI keeps it:
/// the built-in profile's in the config directory its Harness's variables
/// name, an account's in its own config directory, a profile's whose own
/// variables name one there; each with the profile its threads resume on.
#[test]
fn each_profile_history_is_looked_for_where_its_cli_keeps_it() {
    use tcode_core::settings::{EnvVar, ProviderProfile, ProviderSettings, Settings};
    let config_dir = |value: &str| EnvVar {
        name: "CLAUDE_CONFIG_DIR".into(),
        value: value.into(),
        sensitive: false,
    };
    let mut settings = Settings::default();
    settings.provider_mut(ProviderKind::ClaudeCode).shared_env = vec![config_dir("/configured")];
    for (id, profile) in [
        (
            "work",
            ProviderSettings {
                home_path: Some("/profile-homes/work".into()),
                ..ProviderSettings::default()
            },
        ),
        (
            "kimi",
            ProviderSettings {
                env: vec![config_dir("/elsewhere")],
                ..ProviderSettings::default()
            },
        ),
    ] {
        settings.profiles.insert(
            id.into(),
            ProviderProfile {
                kind: ProviderKind::ClaudeCode,
                settings: profile,
            },
        );
    }
    let mut dirs = super::history_dirs(&settings, ProviderKind::ClaudeCode);
    dirs.sort();
    assert_eq!(
        dirs,
        [
            (None, PathBuf::from("/configured")),
            (Some("kimi".into()), PathBuf::from("/elsewhere")),
            (Some("work".into()), PathBuf::from("/profile-homes/work")),
        ]
    );
}
