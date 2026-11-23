use super::*;

/// A draft in worktree mode starts its worktree from the base picked in the composer, not
/// from what the project root has checked out.
#[test]
fn a_worktree_draft_starts_from_the_picked_base() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-worktree-picked-base");
    let repo = test_store.root().join("repo");
    init_git_repo(&repo);
    run_git(&repo, &["switch", "-c", "feature"]).unwrap();
    std::fs::write(repo.join("feature.txt"), "feature\n").unwrap();
    run_git(&repo, &["add", "feature.txt"]).unwrap();
    run_git(&repo, &["commit", "-m", "feature"]).unwrap();
    run_git(&repo, &["switch", "main"]).unwrap();
    let project = Project::from_root(repo.clone());
    test_store.upsert_project(&project).unwrap();
    let (launcher, launches) = recording_launcher();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*test_store).clone());
        state.set_provider_launcher(launcher);
        state
    });

    let Ok(CommandResponse::SessionId(Some(session_id))) = acked(
        &state,
        cx,
        1,
        Command::StartDraft {
            project_id: project.id.clone(),
            cwd: repo.clone(),
        },
    ) else {
        panic!("draft not started");
    };
    for (id, command) in [
        Command::SetDraftWorkspace {
            session_id: session_id.clone(),
            mode: WorkspaceMode::NewWorktree {
                base: "feature".into(),
            },
        },
        Command::SendTurn {
            session_id: session_id.clone(),
            text: "build on the feature".into(),
            attachment_paths: Vec::new(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        acked(&state, cx, id as u64 + 2, command).unwrap();
    }
    next_turn(cx, &launches, 0);

    let cwd = launches.lock().unwrap()[0].0.cwd.clone();
    assert_ne!(cwd, repo);
    assert_eq!(
        std::fs::read_to_string(cwd.join("feature.txt")).unwrap(),
        "feature\n"
    );
    state.read(|state| {
        let meta = state.find_meta(&session_id).unwrap();
        assert_eq!(
            meta.worktree.map(|worktree| worktree.base).as_deref(),
            Some("feature")
        );
    });
    remove_git_worktree(&repo, &cwd).unwrap();
}

/// A thread may move into a worktree of its project's repository that lives outside the
/// project's directory; a directory of no checkout of that repository is still refused. A
/// project rooted in a subdirectory of its repository reaches only that subdirectory of
/// each checkout.
#[test]
fn a_thread_moves_into_a_worktree_of_its_projects_repository() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-move-into-worktree");
    let repo = test_store.root().join("repo");
    init_git_repo(&repo);
    std::fs::create_dir(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/tracked.txt"), "sub\n").unwrap();
    run_git(&repo, &["add", "sub"]).unwrap();
    run_git(&repo, &["commit", "-m", "sub"]).unwrap();
    let outside = test_store.root().join("outside-worktree");
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "elsewhere",
            outside.to_str().unwrap(),
        ],
    )
    .unwrap();
    std::fs::create_dir(outside.join("app")).unwrap();
    std::fs::create_dir(outside.join("other")).unwrap();
    let unrelated = test_store.root().join("unrelated");
    std::fs::create_dir_all(&unrelated).unwrap();
    let project = Project::from_root(repo.clone());
    test_store.upsert_project(&project).unwrap();
    let sub_project = Project::from_root(repo.join("sub"));
    test_store.upsert_project(&sub_project).unwrap();
    for (id, project, cwd) in [
        ("thread", &project, &repo),
        ("sub-thread", &sub_project, &repo.join("sub")),
    ] {
        let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, cwd.clone(), None);
        meta.id = id.into();
        meta.project_id = Some(project.id.clone());
        test_store.upsert_meta(&meta).unwrap();
    }
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let move_to = |thread: &str, project: &Project, cwd: &Path| Command::MoveSession {
        session_id: thread.into(),
        project_id: project.id.clone(),
        cwd: Some(cwd.to_path_buf()),
    };

    let refusal = acked(&state, cx, 1, move_to("thread", &project, &unrelated)).unwrap_err();
    assert_eq!(refusal.code, "cwd_outside_project");
    acked(
        &state,
        cx,
        2,
        move_to("thread", &project, &outside.join("app")),
    )
    .unwrap();
    state.read(|state| {
        assert_eq!(state.find_meta("thread").unwrap().cwd, outside.join("app"));
    });

    for (id, cwd) in [(3, outside.join("other")), (4, repo.clone())] {
        let refusal = acked(&state, cx, id, move_to("sub-thread", &sub_project, &cwd)).unwrap_err();
        assert_eq!(refusal.code, "cwd_outside_project", "{}", cwd.display());
    }
    acked(
        &state,
        cx,
        5,
        move_to("sub-thread", &sub_project, &outside.join("sub")),
    )
    .unwrap();
    state.read(|state| {
        assert_eq!(
            state.find_meta("sub-thread").unwrap().cwd,
            outside.join("sub")
        );
    });
}

#[test]
fn a_subagent_started_in_a_worktree_works_in_it() {
    let cx = &mut TestAppContext::default();
    let root =
        std::env::temp_dir().join(format!("tcode-dispatch-worktree-{}", uuid::Uuid::new_v4()));
    init_git_repo(&root);

    let test_store = TestStore::new("tcode-dispatch-worktree-data");
    let data_worktrees = test_store.root().join("worktrees");
    let project = Project::from_root(root.clone());
    test_store.upsert_project(&project).unwrap();
    stored_thread(
        &test_store,
        "parent",
        ProviderKind::Codex,
        &root,
        |parent| {
            parent.project_id = Some(project.id.clone());
        },
    );
    let (state, providers) = controlled_host(cx, &test_store);
    state.subscribe(cx, Topic::Index);
    let lead = first_turn(&state, cx, &providers, "parent", 0);
    let response = lead
        .control(
            cx,
            "start_thread",
            serde_json::json!({
                "workspace": "worktree",
                "subagent": true,
                "provider": "codex",
                "model": "gpt-6.1-sol",
                "title": "Isolated child",
                "message": "Inspect the workspace",
                "request_id": "isolated",
            }),
        )
        .unwrap();

    let child_id = response["thread_id"].as_str().unwrap().to_string();
    let expected_branch = format!("tcode/{child_id}");
    let expected_path = PathBuf::from(response["cwd"].as_str().unwrap());
    assert!(
        expected_path.starts_with(&data_worktrees) && expected_path.ends_with(&child_id),
        "start_thread response: {response}"
    );
    assert_eq!(response["worktree_branch"], expected_branch);
    cx.run_until_seen(|view| view.session(&child_id).is_some());
    {
        let child = cx.seen().session(&child_id).unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(child.cwd, expected_path);
        assert_eq!(
            child
                .worktree
                .as_ref()
                .map(|worktree| worktree.branch.as_str()),
            Some(expected_branch.as_str())
        );
    }
    assert_eq!(providers.started(cx, 1).options.cwd, expected_path);
    remove_git_worktree(&root, &expected_path).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn orphan_sweep_leaves_worktrees_a_thread_works_in_or_the_user_kept() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orphan-sweep");
    let store = (*test_store).clone();
    let repo = test_store.root().join("repo");
    init_git_repo(&repo);
    let owner = |id: &str| {
        let created = provision(&repo, id, None, store.root()).unwrap();
        let mut meta = SessionMeta::new(ProviderKind::Codex, created.path, None);
        meta.id = id.into();
        meta.worktree = Some(WorktreeInfo {
            root_project_path: repo.clone(),
            base: created.base,
            branch: created.branch,
        });
        meta
    };
    let source = owner("source");
    let kept = owner("kept");
    let orphan = owner("orphan").cwd;
    let mut fork = SessionMeta::new(ProviderKind::Codex, source.cwd.clone(), None);
    fork.id = "fork".into();
    // A thread that owns a worktree it does not work in now.
    let away = owner("away");
    let mut elsewhere = SessionMeta::new(ProviderKind::ClaudeCode, repo.clone(), None);
    elsewhere.id = "elsewhere".into();
    elsewhere
        .other_worktrees
        .push(tcode_core::project::OwnedWorktree {
            path: away.cwd.clone(),
            info: away.worktree.clone().unwrap(),
        });
    for meta in [&source, &kept, &fork, &elsewhere] {
        store.upsert_meta(meta).unwrap();
    }
    let state = cx.new_entity(TestClientState::new(store.clone()));

    state.dispatch_command(
        cx,
        1,
        Command::DeleteSession {
            session_id: "source".into(),
            remove_worktree: true,
        },
    );
    state.dispatch_command(
        cx,
        2,
        Command::DeleteSession {
            session_id: "kept".into(),
            remove_worktree: false,
        },
    );
    cx.run_until_parked();
    assert!(source.cwd.exists(), "the fork still works in it");
    // The next start is past the age that marks a worktree as abandoned.
    let abandoned = std::fs::FileTimes::new()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(2 * 60 * 60));
    for path in [&source.cwd, &kept.cwd, &orphan, &away.cwd] {
        std::fs::set_times(path, abandoned).unwrap();
    }

    let summary = cleanup_orphans(&store, std::time::SystemTime::now()).unwrap();

    assert_eq!(summary.removed, std::slice::from_ref(&orphan));
    assert!(!orphan.exists());
    assert!(source.cwd.join("tracked.txt").exists());
    assert!(kept.cwd.join("tracked.txt").exists());
    assert!(away.cwd.join("tracked.txt").exists());
}
