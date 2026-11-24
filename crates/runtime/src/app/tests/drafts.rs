use super::*;

/// A message goes to the provider with what the user attached to it: the
/// terminal lines they picked and their review comments, in that order after
/// the text, and the images it names; once sent, none of them waits for the
/// next message.
#[test]
fn send_turn_assembles_draft_context_and_attachment_paths() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-send-assembly-test");
    let root = test_store.root().clone();
    let attachment_path = root.join("sample.png");
    std::fs::write(&attachment_path, [1, 2, 3]).unwrap();
    stored_thread(
        &test_store,
        "assembled",
        ProviderKind::Codex,
        &root.join("project"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.open(cx, "assembled");
    state
        .command(
            cx,
            Command::ToggleTerminalPanel {
                session_id: "assembled".into(),
            },
        )
        .unwrap();
    cx.run_until_seen(|view| !view.status("assembled").terminals.is_empty());
    let terminal = cx.seen().status("assembled").terminals[0].clone();
    state
        .command(
            cx,
            Command::CaptureTerminalSelection {
                session_id: "assembled".into(),
                terminal_id: terminal.id,
                selection: Some(tcode_protocol::TerminalSelection {
                    line_start: 12,
                    line_end: 13,
                    text: "cargo test\nok".into(),
                }),
            },
        )
        .unwrap();
    for comment in [
        ReviewComment::new(
            "src/lib.rs".into(),
            7,
            7,
            tcode_core::session::ReviewSide::New,
            "Please fix".into(),
            "let bad = true;".into(),
            "section".into(),
            "Changes".into(),
            3,
            4,
        ),
        ReviewComment::new(
            "src/lib.rs".into(),
            7,
            8,
            tcode_core::session::ReviewSide::New,
            "  Please avoid the unwrap.  ".into(),
            "@@ -7,1 +7,2 @@\n old\n+new".into(),
            "turn:3".into(),
            "Turn 4".into(),
            12,
            13,
        ),
    ] {
        state
            .command(
                cx,
                Command::AddReviewComment {
                    session_id: "assembled".into(),
                    comment,
                },
            )
            .unwrap();
    }
    state
        .command(
            cx,
            Command::SendTurn {
                session_id: "assembled".into(),
                text: "Explain this".into(),
                attachment_paths: vec![attachment_path.clone()],
            },
        )
        .unwrap();

    let SessionCommand::SendTurn {
        text, attachments, ..
    } = providers.started(cx, 0).next_command(cx)
    else {
        panic!("expected the message")
    };
    assert_eq!(
        text,
        format!(
            "Explain this\n\n<terminal_context>\n- {} lines 12-13:\n  12 | cargo test\n  13 | ok\n</terminal_context>\n\n<review_comment sectionId=\"section\" sectionTitle=\"Changes\" filePath=\"src/lib.rs\" startIndex=\"3\" endIndex=\"4\" rangeLabel=\"+7\">\nPlease fix\n```diff\nlet bad = true;\n```\n</review_comment>\n\n<review_comment sectionId=\"turn:3\" sectionTitle=\"Turn 4\" filePath=\"src/lib.rs\" startIndex=\"12\" endIndex=\"13\" rangeLabel=\"+7 to +8\">\nPlease avoid the unwrap.\n```diff\n@@ -7,1 +7,2 @@\n old\n+new\n```\n</review_comment>",
            terminal.title
        )
    );
    assert_eq!(
        attachments,
        vec![Attachment {
            media_type: "image/png".into(),
            data_base64: "AQID".into(),
            source_path: Some(attachment_path.to_string_lossy().into_owned()),
        }]
    );
    let status = cx.seen().status("assembled");
    assert!(status.terminal_contexts.is_empty());
    assert!(status.review_comment_drafts.is_empty());
}

/// A remote client on a bad network can queue a thread's subscription and its
/// unsubscription together and deliver both after it reconnects, although the
/// conversation never loaded on its screen. Only the client's acknowledgement
/// that it showed the thread marks it read.
#[test]
fn only_a_loaded_conversation_marks_a_thread_read() {
    use tcode_protocol::{ClientPayload, Subscription, Topic};
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-read-on-load-test");
    let store = (*test_store).clone();
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/a"), None);
    meta.updated_at = 100;
    store.upsert_meta(&meta).unwrap();
    let id = meta.id.clone();
    let state = cx.new_entity(TestClientState::new(store));
    state.update(cx, |state, _| {
        state.settings.last_visited.insert(id.clone(), 50);
    });
    // Read through the index projection every client shows.
    let unread = |state: &TestEntity| {
        state.read(|state| state.index_snapshot().summary.activity[&id].unread)
    };
    assert!(unread(&state), "updated after the last visit");
    state.read(|state| {
        let activity = &state.index_snapshot().summary.activity[&id];
        assert!(
            !activity.working
                && !activity.turn_running
                && activity.background.is_idle()
                && activity.unsettled == 0
                && !activity.waiting_for_approval
                && !activity.waiting_for_input,
            "a thread with no live provider has no work"
        );
    });

    let subscription = Subscription {
        known_index: None,
        topic: Topic::SessionEvents {
            session_id: id.clone(),
        },
        after: None,
    };
    state.deliver(cx, 1, ClientPayload::Subscribe(subscription.clone()));
    state.deliver(cx, 2, ClientPayload::Unsubscribe(subscription));
    cx.run_until_parked();
    assert!(unread(&state), "a late subscription is not a read");

    let read = |through| Command::MarkSessionRead {
        session_id: id.clone(),
        through,
    };
    state.dispatch_command(cx, 3, read(100));
    assert!(!unread(&state), "read through what the client showed");
    state.dispatch_command(cx, 4, read(80));
    assert!(
        !unread(&state),
        "a delayed older acknowledgement does not rewind"
    );

    state.update(cx, |state, cx| {
        let mut parked = state.sessions.iter().find(|m| m.id == id).cloned().unwrap();
        parked.updated_at = 120;
        state.persist_meta(&parked, cx);
    });
    assert!(unread(&state), "newer than anything the client showed");

    state.update(cx, |state, cx| state.settle_session(&id, cx));
    assert!(!unread(&state), "a settled thread is never unread");
    state.update(cx, |state, cx| state.unsettle_session(&id, cx));
    assert!(
        unread(&state),
        "unsettling restores what the client never saw"
    );
}

#[test]
fn draft_defaults_follow_history_within_current_account_and_model_settings() {
    let store = TestStore::new("tcode-draft-defaults");
    let project = Project::from_root(store.root().join("project"));
    let empty_project = Project::from_root(store.root().join("empty-project"));
    for project in [&project, &empty_project] {
        fs::create_dir_all(&project.root).unwrap();
        store.upsert_project(project).unwrap();
    }
    for (id, provider, project_id, model, updated_at) in [
        (
            "older",
            ProviderKind::Codex,
            project.id.as_str(),
            "older-model",
            100,
        ),
        (
            "project",
            ProviderKind::ClaudeCode,
            project.id.as_str(),
            "team-model",
            500,
        ),
        (
            "global",
            ProviderKind::Acp,
            "other-project",
            "global-model",
            900,
        ),
        (
            "archived",
            ProviderKind::Codex,
            project.id.as_str(),
            "archived-model",
            1000,
        ),
    ] {
        stored_thread(&store, id, provider, &project.root, |meta| {
            meta.project_id = Some(project_id.into());
            meta.model = Some(model.into());
            meta.updated_at = updated_at;
            meta.option_selections = vec![OptionSelection {
                id: agent::EFFORT_OPTION_ID.into(),
                value: serde_json::json!("high"),
            }];
            match id {
                "project" => {
                    meta.profile_id = Some("team".into());
                    meta.option_selections.extend([
                        OptionSelection {
                            id: "permissionMode".into(),
                            value: serde_json::json!("acceptEdits"),
                        },
                        OptionSelection {
                            id: "serviceTier".into(),
                            value: serde_json::json!("fast"),
                        },
                    ]);
                }
                "global" => meta.acp_agent_id = Some("global-agent".into()),
                "archived" => meta.archived_at = Some(1001),
                _ => {}
            }
        });
    }
    let mut settings = Settings {
        auto_settle_after_days: None,
        ..Settings::default()
    };
    settings.profiles.insert(
        "team".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                custom_models: vec!["team-model".into(), "team-next".into()],
                env: vec![tcode_core::settings::EnvVar {
                    name: "ANTHROPIC_API_KEY".into(),
                    value: String::new(),
                    sensitive: true,
                }],
                ..Default::default()
            },
        },
    );
    settings.acp_agents.insert(
        "global-agent".into(),
        InstalledAgent {
            id: "global-agent".into(),
            name: "Global agent".into(),
            version: String::new(),
            icon: None,
            launch: agent::AcpLaunch::Custom {
                command: "global-agent".into(),
                args: Vec::new(),
                env: Vec::new(),
            },
            enabled: true,
            env: Vec::new(),
            launch_args: None,
        },
    );
    tcode_services::settings::SettingsStore::new(store.root().clone())
        .save(&settings)
        .unwrap();
    store
        .save_models(
            ProviderKind::ClaudeCode,
            &[
                agent::ModelSpec {
                    id: "native-default".into(),
                    display_name: "Default".into(),
                    is_default: true,
                    options: Vec::new(),
                },
                agent::ModelSpec {
                    id: "native-other".into(),
                    display_name: "Other".into(),
                    is_default: false,
                    options: Vec::new(),
                },
            ],
        )
        .unwrap();
    let cx = &mut TestAppContext::default();
    let (state, _) = played_host(cx, &store);
    let start = |project: &Project, cwd: PathBuf| Command::StartDraft {
        project_id: project.id.clone(),
        cwd,
    };
    let draft = |response| match response {
        CommandResponse::SessionId(Some(id)) => id,
        other => panic!("draft response: {other:?}"),
    };
    let project_draft = draft(
        state
            .command(cx, start(&project, project.root.clone()))
            .unwrap(),
    );
    state.open(cx, &project_draft);
    let status = cx.seen().status(&project_draft);
    assert_eq!(status.provider, ProviderKind::ClaudeCode);
    assert_eq!(status.requested_profile_id.as_deref(), Some("team"));
    assert_eq!(status.requested_model.as_deref(), Some("team-model"));
    assert!(
        status
            .provider_option_requested_selections
            .iter()
            .any(|option| option.id == agent::EFFORT_OPTION_ID && option.value == "high")
    );
    assert!(
        status
            .provider_option_requested_selections
            .iter()
            .any(|option| option.id == "permissionMode" && option.value == "acceptEdits")
    );
    assert!(
        !status
            .provider_option_requested_selections
            .iter()
            .any(|option| option.id == "serviceTier")
    );
    let global_draft = draft(
        state
            .command(cx, start(&empty_project, empty_project.root.clone()))
            .unwrap(),
    );
    state.open(cx, &global_draft);
    let status = cx.seen().status(&global_draft);
    assert_eq!(status.provider, ProviderKind::Acp);
    assert_eq!(status.requested_model.as_deref(), Some("global-model"));
    assert_eq!(status.acp_agent_id.as_deref(), Some("global-agent"));
    assert!(
        !status
            .provider_option_requested_selections
            .iter()
            .any(|option| option.id == agent::EFFORT_OPTION_ID)
    );

    let patch = |profile: &str, patch| Command::UpdateProfileSettings {
        profile_id: profile.into(),
        patch,
    };
    state
        .command(
            cx,
            patch(
                "team",
                ProfileSettingsPatch::SetHiddenModels(vec!["team-model".into()]),
            ),
        )
        .unwrap();
    let reopened = draft(
        state
            .command(cx, start(&project, project.root.clone()))
            .unwrap(),
    );
    assert_eq!(reopened, project_draft);
    state.open(cx, &reopened);
    let status = cx.seen().status(&reopened);
    assert_eq!(status.requested_profile_id.as_deref(), Some("team"));
    assert_eq!(status.requested_model.as_deref(), Some("team-next"));
    state
        .command(
            cx,
            Command::UpdateAcpAgent {
                id: "global-agent".into(),
                patch: tcode_core::acp::AcpAgentPatch::SetEnabled { enabled: false },
            },
        )
        .unwrap();
    for provider in ProviderKind::NATIVE {
        state
            .command(
                cx,
                patch(
                    Settings::builtin_profile_id(provider),
                    ProfileSettingsPatch::SetEnabled { enabled: false },
                ),
            )
            .unwrap();
    }
    state
        .command(
            cx,
            patch(
                "team",
                ProfileSettingsPatch::SetHiddenModels(vec![
                    "team-model".into(),
                    "team-next".into(),
                ]),
            ),
        )
        .unwrap();
    for cwd in [project.root.clone(), project.root.join("fresh")] {
        let error = state.command(cx, start(&project, cwd)).unwrap_err();
        assert_eq!(error.code, "no_available_model");
    }
    state
        .command(
            cx,
            patch("claude", ProfileSettingsPatch::SetEnabled { enabled: true }),
        )
        .unwrap();
    state
        .command(
            cx,
            patch(
                "claude",
                ProfileSettingsPatch::SetHiddenModels(vec!["native-default".into()]),
            ),
        )
        .unwrap();
    state
        .command(
            cx,
            patch("team", ProfileSettingsPatch::SetEnabled { enabled: false }),
        )
        .unwrap();
    let fresh = draft(
        state
            .command(cx, start(&project, project.root.join("fresh")))
            .unwrap(),
    );
    state.open(cx, &fresh);
    let status = cx.seen().status(&fresh);
    assert_eq!(status.provider, ProviderKind::ClaudeCode);
    assert_eq!(status.requested_profile_id, None);
    assert_eq!(status.requested_model.as_deref(), Some("native-other"));
    state.command(cx, Command::ShutdownAllAndFlush).unwrap();
    let persisted = store.load_index().unwrap();
    for id in [&project_draft, &global_draft, &fresh] {
        assert!(!persisted.iter().any(|meta| &meta.id == id));
    }
}

#[test]
fn draft_model_selection_switches_to_the_rows_explicit_provider() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-draft-provider-selection-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        let mut previous = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/previous"),
            Some("gpt-5.6-sol".into()),
        );
        previous.project_id = Some("project-target".into());
        previous.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("high"),
        });
        state.sessions = vec![previous].into();
        state.start_draft("project-target".into(), PathBuf::from("/tmp/target"), cx);

        let draft = state.selected_session().unwrap();
        assert_eq!(draft.meta.provider, ProviderKind::Codex);
        assert_eq!(draft.meta.model.as_deref(), Some("gpt-5.6-sol"));

        // `claude-fable-5-1` cannot be reliably classified by a hard-coded
        // model-name heuristic. The provider comes from its picker row.
        state.host.set_active_model(
            state.selected.as_deref().unwrap_or_default(),
            ProviderKind::ClaudeCode,
            Some("claude-fable-5-1".into()),
            None,
            cx,
        );

        let draft = state.selected_session().unwrap();
        assert_eq!(draft.meta.provider, ProviderKind::ClaudeCode);
        assert_eq!(draft.meta.model.as_deref(), Some("claude-fable-5-1"));
        assert!(draft.meta.acp_agent_id.is_none());
        assert_eq!(
            draft.meta.option_selections,
            vec![OptionSelection {
                id: "permissionMode".into(),
                value: serde_json::json!("auto"),
            }]
        );
        assert!(state.store.load_index().unwrap().is_empty());
    });
}

#[test]
fn model_switch_restores_last_effort_used_with_that_model() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-model-switch-effort-memory-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(TestClientState::new(store));

    state.update(cx, |state, cx| {
        let mut sol = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/sol"),
            Some("gpt-5.6-sol".into()),
        );
        sol.project_id = Some("project-target".into());
        sol.updated_at = 10;
        sol.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("max"),
        });
        let mut fable = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/fable"),
            Some("claude-fable-5-1".into()),
        );
        fable.project_id = Some("project-target".into());
        fable.updated_at = 20;
        // The same model last ran on another account, at an effort of its own.
        let mut other_account = sol.clone();
        other_account.profile_id = Some("second-account".into());
        other_account.updated_at = 15;
        other_account.option_selections = vec![OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("low"),
        }];
        state.sessions = vec![sol, other_account, fable].into();
        state.start_draft("project-target".into(), PathBuf::from("/tmp/target"), cx);

        // Switching the draft to a model brings back the effort it last ran at
        // on that account, not the model's default.
        state.host.set_active_model(
            state.selected.as_deref().unwrap_or_default(),
            ProviderKind::Codex,
            Some("gpt-5.6-sol".into()),
            None,
            cx,
        );

        let draft = state.selected_session().unwrap();
        assert_eq!(draft.meta.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            draft
                .meta
                .option_selections
                .iter()
                .find(|selection| selection.id == "reasoningEffort")
                .map(|selection| &selection.value),
            Some(&serde_json::json!("max"))
        );

        state.host.set_active_model(
            state.selected.as_deref().unwrap_or_default(),
            ProviderKind::Codex,
            Some("gpt-5.6-sol".into()),
            Some("second-account".into()),
            cx,
        );

        let draft = state.selected_session().unwrap();
        assert_eq!(
            draft
                .meta
                .option_selections
                .iter()
                .find(|selection| selection.id == "reasoningEffort")
                .map(|selection| &selection.value),
            Some(&serde_json::json!("low"))
        );
    });
}

#[test]
fn reopened_command_cache_seeds_a_draft_before_provider_start() {
    let test_store = TestStore::new("tcode-command-seed-test");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    let commands = vec![ProviderCommand {
        name: "review".into(),
        description: Some("Review changes".into()),
        kind: agent::ProviderCommandKind::Command,
    }];
    store
        .save_commands(
            &CommandsCacheKey::Native {
                provider: ProviderKind::ClaudeCode,
                home: None,
            },
            &commands,
        )
        .unwrap();

    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new(
        SessionStore::open_at(root.clone()).unwrap(),
    ));
    state.update(cx, |state, cx| {
        state.start_draft("project".into(), PathBuf::from("/tmp/project"), cx);
        let draft = state.selected_session().unwrap();
        assert_eq!(draft.meta.provider, ProviderKind::ClaudeCode);
        assert_eq!(draft.provider_commands, commands);
    });
}

/// Settings → Harnesses binary and launch arguments reach the spawn options.
#[test]
fn provider_binary_and_launch_arguments_follow_the_selected_profile() {
    let mut settings = Settings::default();
    let claude = settings.provider_mut(ProviderKind::ClaudeCode);
    claude.binary_path = Some(PathBuf::from("/custom/claude"));
    claude.launch_args = Some("--chrome --verbose".into());
    let codex = settings.provider_mut(ProviderKind::Codex);
    codex.binary_path = Some(PathBuf::from("/custom/codex"));
    let pi = settings.provider_mut(ProviderKind::Pi);
    pi.launch_args = Some("--verbose".into());

    let meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        McpServers::default(),
    )
    .unwrap();
    assert_eq!(opts.binary_path, Some(PathBuf::from("/custom/claude")));
    assert_eq!(opts.extra_args, vec!["--chrome", "--verbose"]);

    // This Codex profile has no launch arguments.
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        McpServers::default(),
    )
    .unwrap();
    assert_eq!(opts.binary_path, Some(PathBuf::from("/custom/codex")));
    assert!(opts.extra_args.is_empty());

    // pi project trust is opt-in and appends --approve after launch args.
    let meta = SessionMeta::new(ProviderKind::Pi, PathBuf::from("/x"), None);
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        McpServers::default(),
    )
    .unwrap();
    assert_eq!(opts.extra_args, vec!["--verbose"]);

    settings
        .provider_mut(ProviderKind::Pi)
        .pi
        .trust_project_extensions = true;
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        McpServers::default(),
    )
    .unwrap();
    assert_eq!(opts.extra_args, vec!["--verbose", "--approve"]);
}

/// Sensitive env rows contribute their value from `secrets.json`, never from
/// settings.json (which stores an empty value for them).
/// A third-party Claude profile ("Klaude Kode" → Kimi) launches against its
/// own endpoint, binary, and key, in parallel with the untouched official
/// Claude profile. This is the end-to-end proof of profile-ization at the
/// launch layer.
#[test]
fn third_party_profile_launches_in_parallel_with_builtin() {
    let test_store = TestStore::new("tcode-profile-env");
    let store = (*test_store).clone();
    let mut state = TestClientState::new(store);

    let mut settings = state.settings.clone();
    // Official Claude keeps its own key.
    settings.provider_mut(ProviderKind::ClaudeCode).env = vec![
        EnvVar {
            name: "PLAIN".into(),
            value: "visible".into(),
            sensitive: false,
        },
        EnvVar {
            name: "ANTHROPIC_API_KEY".into(),
            value: String::new(),
            sensitive: true,
        },
        EnvVar {
            name: "UNSET_SECRET".into(),
            value: String::new(),
            sensitive: true,
        },
    ];
    // Every account of the harness gets its shared variables first; an
    // account's own value of the same name wins.
    settings.provider_mut(ProviderKind::ClaudeCode).shared_env = vec![
        EnvVar {
            name: "SHARED".into(),
            value: "all".into(),
            sensitive: false,
        },
        EnvVar {
            name: "SHARED_KEY".into(),
            value: String::new(),
            sensitive: true,
        },
        EnvVar {
            name: "ANTHROPIC_MODEL".into(),
            value: "shared-model".into(),
            sensitive: false,
        },
    ];
    // A user "Klaude Kode" profile pointing at Kimi's Anthropic-compatible
    // endpoint, with its own binary and (sensitive) key.
    settings.profiles.insert(
        "klaude-kode".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                display_name: Some("Klaude Kode".into()),
                env: vec![
                    EnvVar {
                        name: "ANTHROPIC_BASE_URL".into(),
                        value: "https://api.kimi.com/coding/".into(),
                        sensitive: false,
                    },
                    EnvVar {
                        name: "ANTHROPIC_MODEL".into(),
                        value: "k3[1m]".into(),
                        sensitive: false,
                    },
                    EnvVar {
                        name: "ANTHROPIC_API_KEY".into(),
                        value: String::new(),
                        sensitive: true,
                    },
                ],
                binary_path: Some(PathBuf::from("/opt/kimi/claude")),
                ..ProviderSettings::default()
            },
        },
    );
    state.settings = settings;
    state
        .settings_store
        .set_profile_secret(
            Settings::builtin_profile_id(ProviderKind::ClaudeCode),
            "ANTHROPIC_API_KEY",
            Some("sk-official"),
        )
        .unwrap();
    state
        .settings_store
        .set_profile_secret("klaude-kode", "ANTHROPIC_API_KEY", Some("sk-kimi"))
        .unwrap();
    let harness = Settings::harness_secret_id(ProviderKind::ClaudeCode);
    state
        .settings_store
        .set_profile_secret(&harness, "SHARED_KEY", Some("sk-shared"))
        .unwrap();
    let shared = [
        ("SHARED".to_string(), "all".to_string()),
        ("SHARED_KEY".to_string(), "sk-shared".to_string()),
        ("ANTHROPIC_MODEL".to_string(), "shared-model".to_string()),
    ];

    // The profile's launch env carries the shared variables, then the Kimi
    // endpoint + its own key, whose model wins over the shared one.
    let env = launch_env_for_profile(
        &state.settings,
        "klaude-kode",
        state.settings_store.profile_secrets(&harness).unwrap(),
        state.settings_store.profile_secrets("klaude-kode").unwrap(),
    )
    .env;
    assert_eq!(env[..3], shared[..]);
    assert!(env.contains(&(
        "ANTHROPIC_BASE_URL".to_string(),
        "https://api.kimi.com/coding/".to_string()
    )));
    // The child takes the last value a name is given.
    assert_eq!(
        LaunchEnv {
            env: env.clone(),
            home: None
        }
        .child_path(ProviderKind::ClaudeCode, "ANTHROPIC_MODEL"),
        Some(PathBuf::from("k3[1m]"))
    );
    assert!(env.contains(&("ANTHROPIC_API_KEY".to_string(), "sk-kimi".to_string())));

    // The built-in profile is untouched: official key, no third-party URL.
    let builtin_profile_id = Settings::builtin_profile_id(ProviderKind::ClaudeCode);
    let builtin = launch_env_for_profile(
        &state.settings,
        builtin_profile_id,
        state.settings_store.profile_secrets(&harness).unwrap(),
        state
            .settings_store
            .profile_secrets(builtin_profile_id)
            .unwrap(),
    )
    .env;
    assert_eq!(
        builtin,
        shared
            .iter()
            .cloned()
            .chain([
                ("PLAIN".to_string(), "visible".to_string()),
                ("ANTHROPIC_API_KEY".to_string(), "sk-official".to_string()),
            ])
            .collect::<Vec<_>>()
    );
    let cx = TestAppContext::default().host_cx();
    let builtin_meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
    assert_eq!(
        smol::block_on(state.prepare_session_env(&builtin_meta, &cx))
            .unwrap()
            .env,
        builtin
    );
    assert!(!builtin.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));

    // A session bound to the profile resolves the profile's env + binary,
    // while its protocol stays ClaudeCode.
    let mut meta = SessionMeta::new(
        ProviderKind::ClaudeCode,
        PathBuf::from("/x"),
        Some("k3[1m]".into()),
    );
    meta.profile_id = Some("klaude-kode".into());
    assert_eq!(
        state.resolve_profile_binary("klaude-kode"),
        Some(PathBuf::from("/opt/kimi/claude"))
    );
    let launch_env = smol::block_on(state.prepare_session_env(&meta, &cx)).unwrap();
    assert_eq!(launch_env.env, env);
    assert!(!launch_env.env.iter().any(|(key, _)| key == "UNSET_SECRET"));
    assert!(
        launch_env
            .env
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://api.kimi.com/coding/")
    );
    let opts = session_options(&meta, &state.settings, launch_env, McpServers::default()).unwrap();
    assert_eq!(opts.binary_path, Some(PathBuf::from("/opt/kimi/claude")));
}

#[test]
fn session_launch_scopes_mcp_registrations() {
    let registration = |name: &str, port| agent::McpRegistration {
        name: name.into(),
        url: format!("http://127.0.0.1:{port}/mcp"),
        bearer_token: format!("{name}-token"),
    };
    let preview = registration(agent::McpRegistration::SERVER_NAME_PREVIEW, 7);
    let computer = registration(agent::McpRegistration::SERVER_NAME_COMPUTER_USE, 10);
    let control = registration(agent::McpRegistration::SERVER_NAME_CONTROL, 11);
    let visualize = registration(agent::McpRegistration::SERVER_NAME_VISUALIZE, 12);
    for (provider, preview_supported, computer_supported) in [
        (ProviderKind::ClaudeCode, true, true),
        (ProviderKind::Codex, true, false),
    ] {
        let mut settings = Settings::default();
        for computer_enabled in [false, true] {
            settings.computer_use.enabled = computer_enabled;
            let meta = SessionMeta::new(provider, PathBuf::from("/x"), None);
            let options = session_options(
                &meta,
                &settings,
                LaunchEnv::default(),
                McpServers {
                    preview: Some(preview.clone()),
                    computer_use: Some(computer.clone()),
                    control: Some(control.clone()),
                    visualize: Some(visualize.clone()),
                },
            )
            .unwrap();
            let mut expected = Vec::new();
            for (enabled, reg) in [
                (preview_supported, &preview),
                (computer_enabled && computer_supported, &computer),
                (preview_supported, &control),
                (preview_supported, &visualize),
            ] {
                if enabled {
                    expected.push((&reg.name, &reg.url, &reg.bearer_token));
                }
            }
            assert_eq!(
                options
                    .mcp_servers
                    .iter()
                    .map(|reg| (&reg.name, &reg.url, &reg.bearer_token))
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
}
