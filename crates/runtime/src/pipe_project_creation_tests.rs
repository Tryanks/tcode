use super::*;
use tcode_services::user_files::UserDirectories;

/// The host's time in these tests: it stands still, so a run never crosses
/// midnight.
fn clock() -> crate::clock::HostClock {
    crate::clock::ManualClock::starting_at(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000),
    )
    .host_clock()
}

fn fixture(documents: bool) -> (SpawnedHost, std::path::PathBuf) {
    let root =
        std::env::temp_dir().join(format!("tcode-project-creation-{}", uuid::Uuid::new_v4()));
    let store = SessionStore::open_at(root.join("data")).unwrap();
    let host = spawn_host(
        store,
        HostServices {
            user_directories: UserDirectories {
                home: Some(root.join("home")),
                documents: documents.then(|| root.join("documents")),
            },
            clock: clock(),
            ..HostServices::default()
        },
    )
    .unwrap();
    (host, root)
}

#[test]
fn create_new_project_creates_and_registers_a_host_directory() {
    let (host, root) = fixture(true);
    let link = host.link();
    let events = super::p4b_tests::observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    })
    .unwrap();
    let command = Command::CreateNewProject {
        name: "  my-project  ".into(),
        parent: None,
    };
    let CommandResponse::ProjectId(Some(id)) = link.command_blocking(command.clone()).unwrap()
    else {
        panic!("expected project id");
    };
    let expected = root.join("home/TcodeProjects/my-project");
    assert!(expected.is_dir());
    let event = super::tests::next_event(
        &events,
        |event| matches!(&event.event, ServerEvent::IndexUpsertProject(project) if project.id == id),
    );
    let ServerEvent::IndexUpsertProject(project) = event.event else {
        unreachable!()
    };
    assert_eq!(project.root, expected);
    assert!(!project.quick_chat);
    assert_eq!(
        link.command_blocking(command).unwrap(),
        CommandResponse::ProjectId(Some(id))
    );
    for name in ["nested/project", r"nested\project", "", " ", ".", ".."] {
        let error = link
            .command_blocking(Command::CreateNewProject {
                name: name.into(),
                parent: None,
            })
            .unwrap_err();
        assert_eq!(error.code, "invalid_project_root", "name: {name:?}");
    }
    assert!(!root.join("home/TcodeProjects/nested").exists());

    // Settings name the folder new projects go in, `~` being the host's home.
    link.command_blocking(Command::PatchSettings {
        patch: tcode_core::settings::SettingsPatch::NewProjectDir(Some("~/Work".into())),
    })
    .unwrap();
    // A folder the host cannot place is refused, and the last one stays:
    // neither a relative path nor another user's home is a folder here.
    for dir in ["Elsewhere", "~someone/Work"] {
        let error = link
            .command_blocking(Command::PatchSettings {
                patch: tcode_core::settings::SettingsPatch::NewProjectDir(Some(dir.into())),
            })
            .unwrap_err();
        assert_eq!(error.code, "invalid_settings", "dir: {dir:?}");
    }
    link.command_blocking(Command::CreateNewProject {
        name: "elsewhere".into(),
        parent: None,
    })
    .unwrap();
    assert!(root.join("home/Work/elsewhere").is_dir());
    // A folder Add Project names takes the project instead, made as needed.
    link.command_blocking(Command::CreateNewProject {
        name: "chosen".into(),
        parent: Some("~/Given/".into()),
    })
    .unwrap();
    assert!(root.join("home/Given/chosen").is_dir());
    host.shutdown_blocking().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

/// A root a client sends is read by the host's rules: `~` is the host's home
/// and `..` folds, so every way of writing one folder names one project.
#[test]
fn create_project_reads_the_root_as_a_host_path() {
    let (host, root) = fixture(true);
    let link = host.link();
    let events = super::p4b_tests::observed_events(&link);
    link.subscribe(Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    })
    .unwrap();
    let folder = root.join("home/src/app");
    std::fs::create_dir_all(&folder).unwrap();
    let create = |root: std::path::PathBuf| match link
        .command_blocking(Command::CreateProject { root })
        .unwrap()
    {
        CommandResponse::ProjectId(Some(id)) => id,
        other => panic!("expected project id, got {other:?}"),
    };

    let id = create("~/src/app/".into());
    let event = super::tests::next_event(
        &events,
        |event| matches!(&event.event, ServerEvent::IndexUpsertProject(project) if project.id == id),
    );
    let ServerEvent::IndexUpsertProject(project) = event.event else {
        unreachable!()
    };
    assert_eq!(project.root, folder);
    assert_eq!(create(root.join("home/src/other/../app/")), id);
    assert_eq!(create(folder.clone()), id);

    let error = link
        .command_blocking(Command::CreateProject {
            root: "~someone/src".into(),
        })
        .unwrap_err();
    assert_eq!(error.code, "invalid_project_root");
    host.shutdown_blocking().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn scratch_draft_uses_todays_host_documents_directory_and_reuses_the_draft() {
    for documents in [true, false] {
        let (host, root) = fixture(documents);
        let link = host.link();
        let events = super::p4b_tests::observed_events(&link);
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::Index,
            after: None,
        })
        .unwrap();
        super::tests::next_event(&events, |event| {
            matches!(&event.event, ServerEvent::IndexSnapshot(_))
        });
        let CommandResponse::SessionId(Some(id)) =
            link.command_blocking(Command::StartScratchDraft).unwrap()
        else {
            panic!("expected session id");
        };
        let project_root = root.join(if documents {
            "documents/Tcode"
        } else {
            "home/Documents/Tcode"
        });
        let event = super::tests::next_event(&events, |event| {
            matches!(&event.event, ServerEvent::IndexUpsertProject(_))
        });
        let ServerEvent::IndexUpsertProject(project) = event.event else {
            unreachable!()
        };
        assert_eq!(project.root, project_root);
        assert!(
            project.quick_chat,
            "new scratch drafts are replicated as quick chats"
        );
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionStatus {
                session_id: id.clone(),
            },
            after: None,
        })
        .unwrap();
        let session = super::tests::next_event(&events, |event| {
            event.topic
                == (Topic::SessionStatus {
                    session_id: id.clone(),
                })
                && matches!(&event.event, ServerEvent::SessionStatusReplaced(_))
        });
        let ServerEvent::SessionStatusReplaced(status) = session.event else {
            unreachable!()
        };
        assert_eq!(status.cwd, project_root.join(clock().today().to_string()));
        assert_eq!(
            link.command_blocking(Command::StartScratchDraft).unwrap(),
            CommandResponse::SessionId(Some(id))
        );
        assert!(status.cwd.is_dir());
        assert!(status.draft);
        assert_eq!(status.project_id.as_deref(), Some(project.id.as_str()));
        host.shutdown_blocking().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}

/// An upload lands in today's scratch directory, the one quick threads work
/// in, under the name the user gave it and nowhere else whatever the name
/// says. Chunks are taken only in order and only for that thread's reserved
/// files, and the files leave with the thread while the day's folder stays.
#[test]
fn uploads_land_in_the_scratch_directory_and_leave_with_their_thread() {
    let (host, root) = fixture(true);
    let link = host.link();
    let CommandResponse::SessionId(Some(session_id)) =
        link.command_blocking(Command::StartScratchDraft).unwrap()
    else {
        panic!("expected session id");
    };
    let query = |query| smol::block_on(link.query(query));
    let begin = |name: &str| {
        let Ok(QueryResponse::UploadBegun(path)) = query(Query::BeginUpload {
            session_id: session_id.clone(),
            name: name.into(),
            size: 6,
        }) else {
            panic!("upload refused");
        };
        path
    };
    let today = root
        .join("documents/Tcode")
        .join(clock().today().to_string());

    let path = begin("../../notes.txt");
    assert!(path.starts_with(today.join("uploads").join(&session_id)));
    assert_eq!(path.file_name().unwrap(), "notes.txt");
    let write = |path: &std::path::Path, offset, bytes: &[u8]| {
        query(Query::WriteUpload {
            session_id: session_id.clone(),
            path: path.to_owned(),
            offset,
            bytes: bytes.to_vec(),
        })
    };
    assert_eq!(write(&path, 0, b"abc"), Ok(QueryResponse::UploadWritten));
    assert_eq!(write(&path, 3, b"def"), Ok(QueryResponse::UploadWritten));
    assert!(
        write(&path, 3, b"def").is_err(),
        "a stale offset is refused"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");

    let outside = root.join("documents/Tcode/secret.txt");
    std::fs::write(&outside, "secret").unwrap();
    assert!(write(&outside, 6, b"!").is_err());
    assert!(
        query(Query::DiscardUpload {
            session_id: session_id.clone(),
            path: outside.clone(),
        })
        .is_err()
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"secret");
    assert!(
        query(Query::BeginUpload {
            session_id: "no-such-thread".into(),
            name: "a".into(),
            size: 1,
        })
        .is_err()
    );
    assert!(
        query(Query::BeginUpload {
            session_id: session_id.clone(),
            name: "a".into(),
            size: tcode_core::attachments::MAX_UPLOAD_BYTES + 1,
        })
        .is_err()
    );

    let discarded = begin("discarded.txt");
    assert_eq!(
        query(Query::DiscardUpload {
            session_id: session_id.clone(),
            path: discarded.clone(),
        }),
        Ok(QueryResponse::UploadDiscarded)
    );
    assert!(!discarded.parent().unwrap().exists());

    link.command_blocking(Command::DeleteSession {
        session_id: session_id.clone(),
        remove_worktree: false,
    })
    .unwrap();
    // The deletion is answered once the thread's uploads are gone.
    assert!(
        !today.join("uploads").join(&session_id).exists(),
        "the thread's uploads leave with it"
    );
    assert!(today.is_dir(), "quick threads work in the day's folder");
    host.shutdown_blocking().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

/// A project's new thread page is one draft while nothing is typed in it.
/// Text makes it a draft of its own: the index lists it with that text off
/// screen, the next new thread is another, and a restarted host still has
/// it. Emptied, it is the new thread page again. Deleted, it comes back as
/// it was when the deletion is taken back.
#[test]
fn a_draft_holding_text_is_kept_and_the_next_new_thread_is_another() {
    let root = std::env::temp_dir().join(format!("tcode-drafts-{}", uuid::Uuid::new_v4()));
    let project_root = root.join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let spawn = || {
        spawn_host(
            SessionStore::open_at(root.join("data")).unwrap(),
            HostServices {
                clock: clock(),
                ..HostServices::default()
            },
        )
        .unwrap()
    };
    let host = spawn();
    let link = host.link();
    let CommandResponse::ProjectId(Some(project_id)) = link
        .command_blocking(Command::CreateProject {
            root: project_root.clone(),
        })
        .unwrap()
    else {
        panic!("expected project id");
    };
    let start = |link: &HostLink| {
        let CommandResponse::SessionId(Some(id)) = link
            .command_blocking(Command::StartDraft {
                project_id: project_id.clone(),
                cwd: project_root.clone(),
            })
            .unwrap()
        else {
            panic!("expected session id");
        };
        id
    };
    let type_text = |link: &HostLink, id: &str, text: &str| {
        link.command_blocking(Command::SetDraftText {
            session_id: id.into(),
            text: text.into(),
        })
        .unwrap();
    };
    let status = |id: &str| Subscription {
        known_index: None,
        topic: Topic::SessionStatus {
            session_id: id.into(),
        },
        after: None,
    };
    let listed = |link: &HostLink, events: &async_channel::Receiver<EventEnvelope>| {
        let index = Subscription {
            known_index: None,
            topic: Topic::Index,
            after: None,
        };
        link.subscribe(index.clone()).unwrap();
        let snapshot = super::tests::next_event(events, |event| {
            matches!(&event.event, ServerEvent::IndexSnapshot(_))
        });
        link.unsubscribe(index).unwrap();
        let ServerEvent::IndexSnapshot(snapshot) = snapshot.event else {
            unreachable!()
        };
        let mut drafts: Vec<_> = snapshot
            .sessions
            .into_iter()
            .map(|meta| (meta.id, meta.draft_text))
            .collect();
        drafts.sort();
        drafts
    };

    let first = start(&link);
    assert_eq!(start(&link), first, "nothing typed: the same page");
    link.subscribe(status(&first)).unwrap();
    let index = Subscription {
        known_index: None,
        topic: Topic::Index,
        after: None,
    };
    let events = super::p4b_tests::observed_events(&link);
    link.subscribe(index.clone()).unwrap();
    type_text(&link, &first, "half a thought");
    super::tests::next_event(&events, |event| {
        matches!(
            &event.event,
            ServerEvent::IndexUpsertSession(meta)
                if meta.id == first && meta.draft_text.as_deref() == Some("half a thought")
        )
    });
    link.unsubscribe(index).unwrap();
    let second = start(&link);
    assert_ne!(second, first);
    link.subscribe(status(&second)).unwrap();
    type_text(&link, &second, "another");
    type_text(&link, &second, "  ");
    assert_eq!(start(&link), second, "emptied: the new thread page again");
    link.unsubscribe(status(&first)).unwrap();
    link.unsubscribe(status(&second)).unwrap();
    // The command after the unsubscriptions orders the read behind them.
    type_text(&link, &second, "");
    assert_eq!(
        listed(&link, &events),
        [(first.clone(), Some("half a thought".to_string()))]
    );
    host.shutdown_blocking().unwrap();

    let host = spawn();
    let link = host.link();
    let events = super::p4b_tests::observed_events(&link);
    assert_eq!(
        listed(&link, &events),
        [(first.clone(), Some("half a thought".to_string()))]
    );
    let delete = || {
        link.command_blocking(Command::DeleteSession {
            session_id: first.clone(),
            remove_worktree: false,
        })
        .unwrap();
    };
    delete();
    assert_eq!(listed(&link, &events), []);
    // A deletion is taken back whole.
    link.command_blocking(Command::RestoreDraft {
        session_id: first.clone(),
    })
    .unwrap();
    assert_eq!(
        listed(&link, &events),
        [(first.clone(), Some("half a thought".to_string()))]
    );
    delete();
    assert_eq!(listed(&link, &events), []);
    host.shutdown_blocking().unwrap();
    let _ = std::fs::remove_dir_all(root);
}
