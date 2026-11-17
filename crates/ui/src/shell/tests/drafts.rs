use super::*;

/// Choosing a project in the chooser closes it and shows the draft the
/// host names, the same one when asked again; a phone goes to it. The
/// chooser and the draft raise the keyboard only off a phone, where it is
/// typed into at once; there a tap does, and keeps it across a rotation.
/// One project needs no choosing.
#[gpui::test]
fn project_choice_reuses_drafts_and_focuses_only_on_desktop(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    for (mobile, compact, projects) in [
        (false, false, 2),
        (true, false, 2),
        (true, true, 2),
        (true, true, 1),
    ] {
        new_thread_from_projects(cx, mobile, compact, projects);
    }
}

/// The draft an empty workspace falls back on comes up in its pane and
/// leaves a phone on its threads list: nobody asked to go there.
#[gpui::test]
fn a_fallback_draft_leaves_a_phone_on_its_threads(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount(cx);
    as_mobile(cx);
    resize(cx, 393.);
    let project = tcode_core::project::Project {
        id: "project".into(),
        name: "Project".into(),
        root: PANES_CWD.into(),
        quick_chat: false,
        icon_path: None,
        permission_defaults: Default::default(),
        created_at: 0,
    };
    send_events(
        &host,
        vec![
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: Vec::new(),
                    projects: vec![project],
                }),
            ),
        ],
    );
    await_restore_update(&shell, cx, |store, _| store.index_hydrated());
    draw(cx);
    let request = std::iter::from_fn(|| host.outgoing.try_recv().ok())
        .map(|line| decode_client_line(&line).expect("client line"))
        .find(|message| {
            matches!(
                message.payload,
                ClientPayload::Command(Command::StartDraft { .. })
            )
        })
        .expect("the workspace asks for a draft");
    host.incoming
        .try_send(
            encode_line(&HostMessage::Ack {
                id: request.id,
                result: Ok(tcode_protocol::CommandResponse::SessionId(Some(
                    "draft-1".into(),
                ))),
            })
            .unwrap(),
        )
        .unwrap();
    await_restore_update(&shell, cx, |store, _| store.is_thread_open("draft-1"));
    draw(cx);
    assert_eq!(pane_threads(&shell, cx), [Some("draft-1".into())]);
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.destination(cx), Destination::Threads);
    });
}

/// A column showing nothing opens the remembered project's new thread
/// page instead of a dead empty page, though another project is listed
/// first; a host with no project at all is asked for nothing and the
/// column keeps its add-project state.
#[gpui::test]
fn an_empty_column_opens_the_remembered_projects_draft(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let project = |id: &str| crate::store::tests::project_at(id, std::path::Path::new(PANES_CWD));
    for (projects, asked) in [
        (
            vec![project("first"), project("remembered")],
            vec!["remembered"],
        ),
        (Vec::new(), Vec::new()),
    ] {
        let (shell, host, cx) = mount(cx);
        send_events(
            &host,
            vec![
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(tcode_core::settings::Settings {
                        last_project_id: Some("remembered".into()),
                        ..Default::default()
                    }),
                ),
                (
                    Topic::Index,
                    ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        sessions: Vec::new(),
                        projects,
                    }),
                ),
            ],
        );
        await_restore_update(&shell, cx, |store, _| store.index_hydrated());
        draw(cx);
        let drafted: Vec<String> = std::iter::from_fn(|| host.outgoing.try_recv().ok())
            .filter_map(
                |line| match decode_client_line(&line).expect("client line").payload {
                    ClientPayload::Command(Command::StartDraft { project_id, .. }) => {
                        Some(project_id)
                    }
                    _ => None,
                },
            )
            .collect();
        assert_eq!(drafted, asked);
        assert_eq!(pane_threads(&shell, cx), [None]);
    }
}

/// Deleting the project of the draft on screen takes the draft away; the
/// pane takes the draft of a project still listed rather than emptying.
#[gpui::test]
fn deleting_the_shown_drafts_project_opens_another_projects_draft(cx: &mut TestAppContext) {
    use crate::store::tests::{project_at, scratch_root, thread};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let root = scratch_root("remove-shown-draft-project");
    let disk = tcode_services::store::SessionStore::open_at(root.clone()).expect("open test store");
    for project in ["doomed", "kept"] {
        disk.upsert_project(&project_at(project, &root))
            .expect("persist project");
    }
    disk.upsert_meta(&thread(&root, "kept-thread", "kept", None))
        .expect("persist session");
    let (shell, store, host, mut driver, cx) = mount_on_host(cx, disk, None);
    let shows_draft_of = |project: &'static str| {
        let store = store.clone();
        move |cx: &TestAppContext| {
            store.read_with(cx, |store, cx| {
                store.sole_status(cx).is_some_and(|status| {
                    status.draft && status.project_id.as_deref() == Some(project)
                })
            })
        }
    };

    store.update(cx, |store, cx| {
        store.start_draft("doomed".into(), root.clone(), cx)
    });
    driver.wait_until(cx, "draft for the doomed project", shows_draft_of("doomed"));
    store.update(cx, |store, _| store.delete_project("doomed".into()));
    driver.wait_until(cx, "kept project's draft", shows_draft_of("kept"));
    let draft = store.read_with(cx, |store, _| store.sole_thread());
    assert_eq!(pane_threads(&shell, cx), [draft]);

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}

/// Archiving a parent archives its children in one batch. A pane showing
/// one of those children has no visible parent to return to, so it takes
/// the standing draft of the last interacted project: the same draft
/// session id, so its composer state survives.
#[gpui::test]
fn batch_archive_without_a_visible_parent_reopens_the_standing_draft(cx: &mut TestAppContext) {
    use crate::store::tests::{archived, project_at, scratch_root, thread};
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let root = scratch_root("archive-to-draft");
    let disk = tcode_services::store::SessionStore::open_at(root.clone()).expect("open test store");
    // "other" is listed first, so a draft for it proves nothing about the
    // remembered project; "p" is the one the user last worked in.
    disk.upsert_project(&project_at("other", &root.join("other")))
        .expect("persist project");
    disk.upsert_project(&project_at("p", &root))
        .expect("persist project");
    for meta in [
        thread(&root, "parent", "p", None),
        thread(&root, "child", "p", Some("parent")),
    ] {
        disk.upsert_meta(&meta).expect("persist session");
    }
    let (shell, store, host, mut driver, cx) = mount_on_host(cx, disk, None);

    // Let the launch draft settle on the first project before the user
    // goes into "p" themselves.
    driver.wait_until(cx, "launch draft", |cx| {
        store.read_with(cx, |store, _| store.sole_thread().is_some())
    });
    store.update(cx, |store, cx| {
        store.start_draft("p".into(), root.clone(), cx)
    });
    driver.wait_until(cx, "draft for the last project", |cx| {
        store.read_with(cx, |store, cx| {
            store
                .sole_status(cx)
                .is_some_and(|status| status.draft && status.project_id.as_deref() == Some("p"))
        })
    });
    let draft_id = store
        .read_with(cx, |store, _| store.sole_thread())
        .expect("draft shown");
    store.update(cx, |store, cx| {
        store.open_tab_for(&draft_id, RightTab::Diff, cx)
    });

    store.update(cx, |store, cx| store.show_thread("child".into(), cx));
    driver.wait_until(cx, "child shown", |cx| {
        store.read_with(cx, |store, cx| {
            store
                .sole_status(cx)
                .is_some_and(|status| status.session_id == "child")
        })
    });

    store.update(cx, |store, _| store.archive_session("parent".into()));
    driver.wait_until(cx, "the standing draft reopened", |cx| {
        store.read_with(cx, |store, _| {
            store.sole_thread().as_deref() == Some(draft_id.as_str())
        })
    });
    assert_eq!(pane_threads(&shell, cx), [Some(draft_id.clone())]);
    store.read_with(cx, |store, _| {
        assert!(
            store.panel_state_for(Some(&draft_id)).right_panel_open,
            "the reopened draft lost the state the user left in it"
        );
    });
    assert!(archived(cx, &store, "parent"));
    assert!(archived(cx, &store, "child"));

    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(&root);
}
