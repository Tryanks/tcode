use super::*;

#[test]
fn settings_patches_preserve_top_level_and_nested_siblings_over_the_pipe() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-settings-patch-isolation");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        state.update_settings(
            Settings {
                sidebar_collapsed: true,
                ..Settings::default()
            },
            cx,
        )
    });
    cx.run_until_parked();
    cx.drain_outgoing();
    for (id, patch) in [
        (41, SettingsPatch::ThemeMode(ThemeMode::Dark)),
        (
            42,
            SettingsPatch::BrowserHomeUrl(Some("https://example.com".into())),
        ),
        (43, SettingsPatch::BrowserAllowEvaluate(false)),
    ] {
        state.dispatch_command(cx, id, Command::PatchSettings { patch });
    }
    cx.run_until_parked();
    let outgoing = cx.drain_outgoing();
    let accepted: Vec<_> = outgoing
        .iter()
        .filter_map(|message| match message {
            HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(accepted, [41, 42, 43]);
    assert!(outgoing.iter().any(|message| matches!(message,
        HostMessage::Event(EventEnvelope {
            request_id: None, topic: Topic::Settings,
            event: ServerEvent::SettingsReplaced(settings), ..
        }) if settings.sidebar_collapsed && settings.theme_mode == ThemeMode::Dark
            && settings.browser.home_url.as_deref() == Some("https://example.com")
            && !settings.browser.allow_evaluate
    )));
}

#[test]
fn reset_settings_clears_preferences_but_keeps_credentials_installs_and_unknown_keys() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-reset-settings-scope-test");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    // Preferences the reset must clear.
    let mut settings = Settings {
        theme_mode: ThemeMode::Dark,
        language: Some("zh-CN".into()),
        word_wrap_diffs: true,
        sidebar_provider_marks: true,
        ..Settings::default()
    };
    settings.browser.home_url = Some("https://example.com".into());
    // Everything below cost the user a login, a download, or a newer build.
    settings
        .provider_mut(ProviderKind::Codex)
        .env
        .push(tcode_core::settings::EnvVar {
            name: "OPENAI_API_KEY".into(),
            value: "secret".into(),
            sensitive: true,
        });
    settings.profiles.insert(
        "work-claude".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings::default(),
        },
    );
    settings.codex_binary = Some(PathBuf::from("/custom/codex"));
    settings.claude_binary = Some(PathBuf::from("/custom/claude"));
    settings.acp_agents.insert(
        "first".into(),
        InstalledAgent {
            id: "first".into(),
            name: "First".into(),
            version: "1.2.3".into(),
            icon: None,
            launch: agent::AcpLaunch::Npx {
                package: "first-agent".into(),
                args: Vec::new(),
                env: Vec::new(),
            },
            enabled: true,
            env: Vec::new(),
            launch_args: None,
            http_mcp: false,
        },
    );
    settings.collapsed_projects.push("project".into());
    let favorite = FavoriteModel::Row {
        provider: ProviderKind::Codex,
        profile_id: Some("work-codex".into()),
        model: "gpt-5.6-sol".into(),
    };
    settings.favorite_models.push(favorite.clone());
    settings.sidebar_collapsed = true;
    settings.project_sort = tcode_core::settings::ProjectSort::NameAsc;
    settings.sidebar_layout = tcode_core::settings::SidebarLayout::Grouped;
    settings.last_visited.insert("session".into(), 42);
    settings
        .unknown
        .insert("future_key".into(), serde_json::json!({"kept": true}));

    state.update(cx, |state, cx| {
        state.update_settings(settings, cx);
        state.reset_settings(cx);

        let reset = &state.settings;
        assert_eq!(reset.theme_mode, ThemeMode::System);
        assert_eq!(reset.language, None);
        assert!(!reset.word_wrap_diffs);
        assert!(!reset.sidebar_provider_marks);
        assert_eq!(reset.browser.home_url, None);

        assert_eq!(
            reset.provider(ProviderKind::Codex).env[0].value,
            "secret",
            "provider credentials must survive a restore"
        );
        assert!(reset.profiles.contains_key("work-claude"));
        assert_eq!(reset.codex_binary, Some(PathBuf::from("/custom/codex")));
        assert_eq!(reset.claude_binary, Some(PathBuf::from("/custom/claude")));
        assert!(reset.acp_agents.contains_key("first"));
        assert_eq!(reset.collapsed_projects, vec!["project".to_string()]);
        assert_eq!(reset.favorite_models, [favorite]);
        assert!(reset.sidebar_collapsed);
        assert_eq!(
            reset.project_sort,
            tcode_core::settings::ProjectSort::NameAsc
        );
        assert_eq!(
            reset.sidebar_layout,
            tcode_core::settings::SidebarLayout::Grouped
        );
        assert_eq!(reset.last_visited.get("session"), Some(&42));
        assert_eq!(
            reset.unknown.get("future_key"),
            Some(&serde_json::json!({"kept": true})),
            "forward-compat keys must not be destroyed by a restore"
        );
    });
}

/// A secrets.json this build cannot read at start: the host runs, clients
/// are told the saved secrets could not be read rather than shown none,
/// saving a secret says why it is refused, and the file is left as it is.
#[test]
fn unreadable_secrets_are_reported_and_never_written_over() {
    let test_store = TestStore::new("tcode-secrets-unreadable");
    std::fs::create_dir_all(test_store.root()).unwrap();
    let torn = r#"{"version":1,"profiles":{"codex":"#;
    std::fs::write(test_store.root().join("secrets.json"), torn).unwrap();
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    state.deliver(
        cx,
        1000,
        tcode_protocol::ClientPayload::Subscribe(Subscription {
            topic: Topic::Providers,
            after: None,
            known_index: None,
        }),
    );
    cx.run_until_parked();
    let unreadable = cx
        .drain_outgoing()
        .into_iter()
        .find_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                event: ServerEvent::ProvidersReplaced(status),
                ..
            }) => Some(status.secrets_unreadable),
            _ => None,
        });
    assert!(
        unreadable
            .flatten()
            .is_some_and(|error| error.contains("secrets.json")),
        "the providers status says the secrets could not be read"
    );
    state.dispatch_command(
        cx,
        43,
        Command::SetProfileSecret {
            profile_id: "codex".into(),
            name: "OPENAI_API_KEY".into(),
            value: Some("typed-again".into()),
        },
    );
    cx.run_until_parked();
    let outgoing = cx.drain_outgoing();
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            event: ServerEvent::Runtime(RuntimeEvent::Error(RuntimeError::FileUnreadable { error })),
            ..
        }) if error.contains("secrets.json")
    )));
    assert!(!outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            event: ServerEvent::ProvidersReplaced(status),
            ..
        }) if status.secret_names.get("codex").is_some_and(|names| !names.is_empty())
    )));
    state.update(cx, |state, _| state.close_store().unwrap());
    assert_eq!(
        std::fs::read_to_string(test_store.root().join("secrets.json")).unwrap(),
        torn
    );
}

/// A finished turn refreshes the account usage of the profile it ran on and
/// of no other, and asks the thread's own running harness for it instead of
/// starting another. Claude Code reports `max` for the Ultra plan too, so the
/// plan is the one its account file names.
#[test]
fn a_turn_refreshes_only_its_profiles_usage_from_its_running_harness() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-turn-usage");
    let account = test_store.root().join("account");
    std::fs::create_dir_all(&account).unwrap();
    std::fs::write(
        account.join(".claude.json"),
        r#"{"oauthAccount":{"organizationType":"claude_max","planDisplayName":"Ultra"}}"#,
    )
    .unwrap();
    let mut settings = Settings::default();
    settings.provider_mut(ProviderKind::ClaudeCode).home_path = Some(account);
    SettingsStore::new(test_store.root().clone())
        .save(&settings)
        .unwrap();
    stored_thread(
        &test_store,
        "thread",
        ProviderKind::ClaudeCode,
        &test_store.root().join("project"),
        |_| {},
    );
    let (state, providers) = played_host(cx, &test_store);
    state.subscribe(cx, Topic::Providers);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    provider.play([turn_completed("turn", TurnStatus::Completed)]);
    let reply = loop {
        if let SessionCommand::ReadAccountUsage { reply } = cx.run_until_recv(&provider.commands) {
            break reply;
        }
    };
    cx.run_until_parked();
    assert_eq!(
        cx.seen().providers().usage_checking,
        HashSet::from(["claude".to_string()])
    );
    reply
        .try_send(Ok(serde_json::json!({
            "subscription_type": "max",
            "rate_limits_available": true,
            "rate_limits": {"five_hour": {"utilization": 94, "resets_at": "2026-09-04T21:20:00+00:00"}}
        })))
        .unwrap();
    cx.run_until_seen(|view| view.providers().usage_checking.is_empty());
    let usage = &cx.seen().providers().provider_usage;
    assert_eq!(usage.keys().collect::<Vec<_>>(), ["claude"]);
    assert_eq!(usage["claude"].plan.as_deref(), Some("Ultra"));
    assert_eq!(usage["claude"].windows.len(), 1);
    assert_eq!(providers.count(), 1, "no other harness is started for it");
}

/// Another Claude account starts from a copy of the user's own configuration,
/// never the default account's sign-in, endpoint or history. A provider
/// without account sign-in is refused.
#[cfg(unix)]
#[test]
fn another_claude_account_inherits_configuration_but_not_the_account() {
    let test_store = TestStore::new("tcode-account-profile-test");
    let default = test_store.root().join("claude-default");
    let shared = test_store.root().join("shared");
    for (path, contents) in [
        (
            default.join("settings.json"),
            r#"{"model":"opus","apiKeyHelper":"/bin/key","permissions":{"allow":["Bash(ls)"]},"env":{"ANTHROPIC_API_KEY":"sk","ANTHROPIC_AUTH_TOKEN":"t","ANTHROPIC_BASE_URL":"https://proxy.test","FOO":"bar"}}"#,
        ),
        (default.join("commands/ship.md"), "Ship it."),
        (default.join("skills/review/SKILL.md"), "Review the diff."),
        (
            default.join("plugins/known_marketplaces.json"),
            r#"{"official":{}}"#,
        ),
        (default.join(".credentials.json"), r#"{"claudeAiOauth":{}}"#),
        (
            default.join(".claude.json"),
            r#"{"oauthAccount":{"emailAddress":"work@example.com"}}"#,
        ),
        (default.join("projects/-repo/x.jsonl"), "{}"),
        (default.join("history.jsonl"), "{}"),
        (shared.join("CLAUDE.md"), "Answer briefly."),
        (shared.join("agents/reviewer.md"), "Review."),
        (shared.join("skills/linked/SKILL.md"), "Linked."),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    // Dotfile managers link files and directories in, relative links
    // included, and a link may point back up its own tree.
    std::os::unix::fs::symlink("../shared/CLAUDE.md", default.join("CLAUDE.md")).unwrap();
    std::os::unix::fs::symlink(shared.join("agents"), default.join("agents")).unwrap();
    std::os::unix::fs::symlink("../../shared/skills/linked", default.join("skills/linked"))
        .unwrap();
    std::os::unix::fs::symlink("..", default.join("skills/review/up")).unwrap();
    let mut host = TestClientState::new((*test_store).clone());
    let claude = host.settings.provider_mut(ProviderKind::ClaudeCode);
    // The sign-in the new profile starts must not reach a `claude` on PATH.
    claude.binary_path = Some(test_store.root().join("no-claude"));
    claude.home_path = Some(default.clone());
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(host);
    let create = |provider| Command::CreateAccountProfile {
        provider,
        name: "Claude Personal".into(),
        inherit_config: true,
    };

    state.dispatch_command(cx, 1, create(ProviderKind::Pi));
    let (code, _) = cx.run_until_sent(|message| match message {
        HostMessage::Ack {
            id: 1,
            result: Err(error),
        } => Some(error.code.clone()),
        _ => None,
    });
    assert_eq!(code, "no_account_login");

    state.dispatch_command(cx, 2, create(ProviderKind::ClaudeCode));
    let (id, _) = cx.run_until_sent(|message| match message {
        HostMessage::Ack {
            id: 2,
            result: Ok(CommandResponse::ProfileId(id)),
        } => Some(id.clone()),
        _ => None,
    });
    let home = test_store.root().join("profile-homes").join(&id);
    let profile = state.read(|state| state.settings.resolved_profile(&id).unwrap());
    assert_eq!(profile.kind, ProviderKind::ClaudeCode);
    assert_eq!(
        profile.settings.display_name.as_deref(),
        Some("Claude Personal")
    );
    assert_eq!(profile.settings.home_path.as_ref(), Some(&home));
    assert_eq!(
        std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        r#"{"hasCompletedOnboarding":true}"#
    );
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings,
        serde_json::json!({
            "model": "opus",
            "permissions": {"allow": ["Bash(ls)"]},
            "env": {"FOO": "bar"},
        })
    );
    for (copied, contents) in [
        ("CLAUDE.md", "Answer briefly."),
        ("agents/reviewer.md", "Review."),
        ("commands/ship.md", "Ship it."),
        ("skills/review/SKILL.md", "Review the diff."),
        ("skills/linked/SKILL.md", "Linked."),
    ] {
        assert_eq!(
            std::fs::read_to_string(home.join(copied)).unwrap(),
            contents,
            "{copied}"
        );
    }
    for copy in ["CLAUDE.md", "agents", "skills/linked"] {
        assert!(
            !std::fs::symlink_metadata(home.join(copy))
                .unwrap()
                .is_symlink(),
            "{copy} must be a copy, not a link"
        );
    }
    for kept in [
        ".credentials.json",
        "projects",
        "history.jsonl",
        "plugins",
        "skills/review/up",
    ] {
        assert!(!home.join(kept).exists(), "{kept} must not be copied");
    }
}

/// Another Codex account gets its own `CODEX_HOME` with a copy of the user's
/// configuration, never the default account's sign-in or sessions.
#[test]
fn another_codex_account_inherits_configuration_but_not_the_account() {
    let test_store = TestStore::new("tcode-codex-account-test");
    // The sign-in the new profile starts must not reach a `codex` on PATH.
    let binary = test_store.root().join("no-codex");
    let default = test_store.root().join("codex-default");
    for (path, contents) in [
        (default.join("config.toml"), "model = \"gpt-6\"\n"),
        (default.join("AGENTS.md"), "Answer briefly."),
        (default.join("auth.json"), r#"{"tokens":{}}"#),
        (default.join("sessions/rollout.jsonl"), "{}"),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    let mut host = TestClientState::new((*test_store).clone());
    let codex = host.settings.provider_mut(ProviderKind::Codex);
    codex.binary_path = Some(binary);
    codex.home_path = Some(default.clone());
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(host);
    state.dispatch_command(
        cx,
        1,
        Command::CreateAccountProfile {
            provider: ProviderKind::Codex,
            name: "Codex Work".into(),
            inherit_config: true,
        },
    );
    let (id, _) = cx.run_until_sent(|message| match message {
        HostMessage::Ack {
            id: 1,
            result: Ok(CommandResponse::ProfileId(id)),
        } => Some(id.clone()),
        _ => None,
    });
    let home = test_store.root().join("profile-homes").join(&id);
    assert_eq!(
        state.read(|state| state
            .settings
            .resolved_profile(&id)
            .unwrap()
            .settings
            .home_path),
        Some(home.clone())
    );
    for (copied, contents) in [
        ("config.toml", "model = \"gpt-6\"\n"),
        ("AGENTS.md", "Answer briefly."),
    ] {
        assert_eq!(
            std::fs::read_to_string(home.join(copied)).unwrap(),
            contents
        );
    }
    for kept in ["auth.json", "sessions"] {
        assert!(!home.join(kept).exists(), "{kept} must not be copied");
    }
}

/// Each settings edit writes the one field it names: the harness's own
/// fields on its built-in profile, where an account without its own follows
/// them, and never the config directory an account's sign-in lives in.
#[test]
fn a_settings_edit_writes_the_field_it_names() {
    let test_store = TestStore::new("tcode-profile-patch-test");
    let mut host = TestClientState::new((*test_store).clone());
    let home = test_store.root().join("profile-homes").join("work");
    host.settings.profiles.insert(
        "work".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                home_path: Some(home.clone()),
                ..ProviderSettings::default()
            },
        },
    );
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(host);
    let var = |name: &str, value: &str| EnvVar {
        name: name.into(),
        value: value.into(),
        sensitive: false,
    };
    for (id, (profile, patch)) in [
        (
            "claude",
            ProfileSettingsPatch::SetBinaryPath(Some("/opt/claude".into())),
        ),
        (
            "work",
            ProfileSettingsPatch::SetHomePath(Some("/elsewhere".into())),
        ),
        (
            "claude",
            ProfileSettingsPatch::SetHomePath(Some("/home/.claude".into())),
        ),
        (
            "work",
            ProfileSettingsPatch::SetSharedEnv(vec![var("A", "1")]),
        ),
        (
            "claude",
            ProfileSettingsPatch::SetSharedEnv(vec![var(" A ", "1"), var("", "x")]),
        ),
        (
            "work",
            ProfileSettingsPatch::SetDisplayName(Some("  Office ".into())),
        ),
        (
            "pi",
            ProfileSettingsPatch::SetToggle {
                id: "trust_project_extensions".into(),
                enabled: true,
            },
        ),
        (
            "claude",
            ProfileSettingsPatch::SetHarnessEnabled { enabled: false },
        ),
    ]
    .into_iter()
    .enumerate()
    {
        state.dispatch_command(
            cx,
            id as u64 + 1,
            Command::UpdateProfileSettings {
                profile_id: profile.into(),
                patch,
            },
        );
    }
    cx.run_until_parked();
    state.read(|state| {
        let claude = state.settings.provider(ProviderKind::ClaudeCode);
        let work = state.settings.resolved_profile("work").unwrap();
        assert_eq!(claude.binary_path, Some(PathBuf::from("/opt/claude")));
        assert_eq!(
            work.settings.binary_path,
            Some(PathBuf::from("/opt/claude"))
        );
        assert_eq!(state.settings.profiles["work"].settings.binary_path, None);
        assert_eq!(work.settings.home_path, Some(home.clone()));
        assert_eq!(claude.home_path, Some(PathBuf::from("/home/.claude")));
        assert_eq!(claude.shared_env, vec![var("A", "1")]);
        assert_eq!(work.shared_env, vec![var("A", "1")]);
        assert!(work.settings.shared_env.is_empty());
        assert_eq!(work.settings.display_name.as_deref(), Some("Office"));
        assert!(
            state
                .settings
                .provider(ProviderKind::Pi)
                .pi
                .trust_project_extensions
        );
        assert!(!claude.enabled && !work.settings.enabled);
        assert!(state.settings.provider(ProviderKind::Codex).enabled);
    });
}

#[test]
fn installed_acp_duplicates_of_native_providers_only_serve_their_existing_sessions() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-native-acp-duplicates");
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));
    let installed = |id: &str| InstalledAgent {
        id: id.into(),
        name: id.into(),
        version: String::new(),
        icon: None,
        launch: agent::AcpLaunch::Custom {
            command: id.into(),
            args: Vec::new(),
            env: Vec::new(),
        },
        enabled: true,
        env: Vec::new(),
        launch_args: None,
        http_mcp: false,
    };
    let draft = state.update(cx, |state, cx| {
        for id in ["cursor", "grok-build", "gemini"] {
            state.settings.acp_agents.insert(id.into(), installed(id));
        }
        state.start_draft("project".into(), PathBuf::from("/tmp/project"), cx);
        state.selected.clone().unwrap()
    });
    for (request, id, selected) in [
        (1, "cursor", None),
        (2, "grok-build", None),
        (3, "gemini", Some("gemini")),
    ] {
        state.dispatch_command(
            cx,
            request,
            Command::SetActiveAcpAgent {
                session_id: draft.clone(),
                id: id.into(),
            },
        );
        state.read(|state| {
            assert_eq!(
                state.resident(&draft).unwrap().meta.acp_agent_id.as_deref(),
                selected,
                "{id}"
            );
        });
    }

    let mut existing = SessionMeta::new(ProviderKind::Acp, PathBuf::from("/tmp/project"), None);
    existing.acp_agent_id = Some("cursor".into());
    state.read(|state| {
        let opts = session_options(
            &existing,
            &state.settings,
            LaunchEnv::default(),
            McpServers::default(),
        )
        .unwrap();
        assert_eq!(opts.acp.map(|agent| agent.id).as_deref(), Some("cursor"));
    });
}

#[test]
fn marketplace_items_are_runtime_owned_views() {
    let test_store = TestStore::new("tcode-marketplace-view-test");
    let store = (*test_store).clone();
    let mut state = TestClientState::new(store);
    *state.acp_registry = Some(
        serde_json::from_value(serde_json::json!({
            "agents": [
                {
                    "id": "first",
                    "name": "First",
                    "version": "1.2.3",
                    "description": "Supported agent",
                    "distribution": { "npx": { "package": "first-agent" } }
                },
                {
                    "id": "claude-acp",
                    "name": "Hidden",
                    "distribution": { "npx": { "package": "hidden-agent" } }
                },
                {
                    "id": "last",
                    "name": "Last",
                    "version": "4.5.6",
                    "description": "Unsupported agent",
                    "distribution": {}
                }
            ]
        }))
        .unwrap(),
    );
    state.settings.acp_agents.insert(
        "first".into(),
        InstalledAgent {
            id: "first".into(),
            name: "First".into(),
            version: "1.2.3".into(),
            icon: None,
            launch: agent::AcpLaunch::Npx {
                package: "first-agent".into(),
                args: Vec::new(),
                env: Vec::new(),
            },
            enabled: true,
            env: Vec::new(),
            launch_args: None,
            http_mcp: false,
        },
    );
    state.acp_installing.insert(
        "last".into(),
        acp::AcpInstall {
            operation: RuntimeOperationId(1),
            name: "Last".into(),
            cancelled: Default::default(),
        },
    );

    assert_eq!(
        state.acp_marketplace_items(),
        vec![
            AcpMarketplaceItem {
                id: "first".into(),
                name: "First".into(),
                version: "1.2.3".into(),
                description: "Supported agent".into(),
                installed: true,
                installing: false,
                supported: true,
            },
            AcpMarketplaceItem {
                id: "last".into(),
                name: "Last".into(),
                version: "4.5.6".into(),
                description: "Unsupported agent".into(),
                installed: false,
                installing: true,
                supported: false,
            },
        ]
    );
}

/// A settings file this build cannot read — one a newer build wrote, opened
/// by an older install on the same machine — is never written over: the
/// host runs on defaults, every change it refuses says why, and the file
/// stays for the build that reads it.
#[test]
fn an_unreadable_settings_file_is_never_written_over() {
    let store = TestStore::new("tcode-unreadable-settings");
    let path = store.root().join("settings.json");
    // A theme a newer build added.
    let newer = r#"{"profiles":{"work":{"kind":"claude_code","display_name":"Work"}},"theme_mode":"sepia"}"#;
    std::fs::write(&path, newer).unwrap();
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    assert!(state.read(|state| state.settings.profiles.is_empty()));

    for (id, wrap) in [(1, true), (2, false)] {
        state.dispatch_command(
            cx,
            id,
            Command::PatchSettings {
                patch: SettingsPatch::WordWrapDiffs(wrap),
            },
        );
    }
    cx.run_until_parked();
    let refusals = cx
        .drain_outgoing()
        .into_iter()
        .filter(|message| {
            matches!(
                message,
                HostMessage::Event(EventEnvelope {
                    event: ServerEvent::Runtime(RuntimeEvent::Error(
                        RuntimeError::SettingsUnreadable { .. }
                    )),
                    ..
                })
            )
        })
        .count();
    assert_eq!(refusals, 2);
    state.update(cx, |state, _| state.close_store().unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
}

/// A deleted profile's id comes back with the next profile of that name, which
/// owns none of its stars; other accounts' stars and bare ids stay.
/// A directory already in `profile-homes` — the config directory of an
/// account whose profile the settings no longer hold — is that account's: a
/// new profile of the same name gets a directory of its own, and deleting the
/// new profile leaves the old directory as it was.
#[test]
fn a_new_profile_never_takes_over_a_directory_left_in_profile_homes() {
    let store = TestStore::new("tcode-profile-home-left");
    let left = store.root().join("profile-homes").join("work");
    let account = r#"{"oauthAccount":{"emailAddress":"work@example.com"}}"#;
    std::fs::create_dir_all(left.join("projects/-repo")).unwrap();
    std::fs::write(left.join(".claude.json"), account).unwrap();
    std::fs::write(left.join("projects/-repo/session.jsonl"), "{}").unwrap();
    let cx = &mut TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));

    state.dispatch_command(
        cx,
        1,
        Command::CreateThirdPartyProfile {
            provider: ProviderKind::ClaudeCode,
            name: "Work".into(),
            base_url: "https://proxy.test".into(),
            model: None,
            api_key: "sk-test".into(),
            inherit_config: false,
        },
    );
    cx.run_until(|state| !state.settings.profiles.is_empty());
    let (id, home) = state.read(|state| {
        let (id, profile) = state.settings.profiles.iter().next().unwrap();
        (id.clone(), profile.settings.home_path.clone().unwrap())
    });
    assert_ne!(home, left);
    assert_eq!(home, store.root().join("profile-homes").join(&id));
    assert!(home.join(".claude.json").is_file());

    state.dispatch_command(cx, 2, Command::DeleteProfile { profile_id: id });
    answered(cx, 2);
    assert!(!home.exists());
    assert_eq!(
        std::fs::read_to_string(left.join(".claude.json")).unwrap(),
        account
    );
    assert!(left.join("projects/-repo/session.jsonl").is_file());
}

#[test]
fn deleting_a_profile_takes_its_stars_with_it() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-delete-profile-stars");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, cx| {
        state.settings.profiles.insert(
            "second-account".into(),
            ProviderProfile {
                kind: ProviderKind::Codex,
                settings: Default::default(),
            },
        );
        for profile_id in [Some("second-account".to_owned()), None] {
            state.toggle_favorite_model(ProviderKind::Codex, profile_id, "model".into(), cx);
        }
        state
            .settings
            .favorite_models
            .push(FavoriteModel::Id("older".into()));

        state.delete_profile("second-account", cx).detach();

        assert_eq!(
            state.settings.favorite_models,
            [
                FavoriteModel::Row {
                    provider: ProviderKind::Codex,
                    profile_id: None,
                    model: "model".into(),
                },
                FavoriteModel::Id("older".into()),
            ]
        );
    });
}

/// Verifying a token saves it only when the server takes it: one the host cannot ask the
/// server about stays unsaved, and saving it anyway leaves it saved and unverified.
#[test]
fn a_token_the_server_cannot_be_asked_about_is_saved_only_when_saved_anyway() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-verify-host-token");
    let secrets = || -> serde_json::Value {
        std::fs::read(store.root().join("secrets.json"))
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .unwrap_or_default()
    };
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        state.forge = tcode_services::forge::connect(SettingsStore::new(store.root().clone()), []);
    });
    // Nothing listens on port 1, so the server answers nothing.
    let host = "127.0.0.1:1";
    state.dispatch_command(
        cx,
        701,
        Command::PatchSettings {
            patch: SettingsPatch::SourceControlHost {
                host: host.into(),
                kind: tcode_core::pull_request::HostKind::Gitlab,
                enabled: Some(true),
                account: None,
            },
        },
    );
    state.dispatch_command(
        cx,
        702,
        Command::VerifyHostToken {
            host: host.into(),
            token: "unasked-token".into(),
            email: None,
        },
    );
    let (result, _) = cx.run_until_sent(|message| match message {
        HostMessage::Ack { id: 702, result } => Some(result.clone()),
        _ => None,
    });
    assert_eq!(
        result,
        Ok(CommandResponse::TokenCheck(
            tcode_core::settings::TokenCheck::Unreachable { status: None }
        ))
    );
    assert!(secrets().get("@gitlab").is_none());
    state.dispatch_command(
        cx,
        703,
        Command::SetHostToken {
            host: host.into(),
            token: Some("unasked-token".into()),
            email: None,
        },
    );
    cx.run_until(|state| {
        state
            .settings_snapshot()
            .source_control
            .status
            .get(host)
            .is_some_and(|status| {
                status.problem == Some(tcode_core::settings::HostProblem::Unverified)
            })
    });
    assert_eq!(secrets()["@gitlab"][host], "unasked-token");
}

#[test]
fn github_secret_command_persists_separately_and_settings_never_replicate_it() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-github-secret-command");
    let settings_store = SettingsStore::new(store.root().clone());
    let persisted = || -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(store.root().join("secrets.json")).unwrap()).unwrap()
    };
    let mut legacy_settings = Settings::default();
    legacy_settings.profiles.insert(
        "github".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings::default(),
        },
    );
    settings_store.save(&legacy_settings).unwrap();
    std::fs::write(
        store.root().join("secrets.json"),
        r#"{"github":{"ANTHROPIC_API_KEY":"legacy-provider-secret"}}"#,
    )
    .unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        state.forge = tcode_services::forge::connect(SettingsStore::new(store.root().clone()), []);
    });
    state.dispatch_command(
        cx,
        638,
        Command::SetHostToken {
            host: "GITHUB.COM".into(),
            token: Some("github-writer-secret".into()),
            email: None,
        },
    );
    cx.run_until(|_| persisted()["@github"]["github.com"] == "github-writer-secret");
    // Older builds read secrets.json as profile maps and keep entries they do not know.
    serde_json::from_value::<
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    >(persisted())
    .unwrap();
    assert_eq!(
        settings_store.profile_secrets("github").unwrap()["ANTHROPIC_API_KEY"],
        "legacy-provider-secret"
    );
    state.dispatch_command(
        cx,
        642,
        Command::SetProfileSecret {
            profile_id: "github".into(),
            name: "ANTHROPIC_API_KEY".into(),
            value: Some("provider-replacement-secret".into()),
        },
    );
    cx.run_until(|_| {
        settings_store
            .profile_secrets("github")
            .unwrap()
            .get("ANTHROPIC_API_KEY")
            .map(String::as_str)
            == Some("provider-replacement-secret")
    });
    assert_eq!(persisted()["@github"]["github.com"], "github-writer-secret");
    state.dispatch_command(
        cx,
        639,
        Command::PatchSettings {
            patch: SettingsPatch::SourceControlHost {
                host: "github.com".into(),
                kind: tcode_core::pull_request::HostKind::Github,
                enabled: Some(false),
                account: Some(Some("sample".into())),
            },
        },
    );
    state.dispatch_command(
        cx,
        640,
        Command::PatchSettings {
            patch: SettingsPatch::SourceControlHost {
                host: "git.example.com".into(),
                kind: tcode_core::pull_request::HostKind::Github,
                enabled: Some(true),
                account: None,
            },
        },
    );
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let snapshot = state.settings_snapshot();
        assert!(!snapshot.source_control.hosts["github.com"].enabled);
        assert_eq!(
            snapshot.source_control.hosts["github.com"]
                .account
                .as_deref(),
            Some("sample")
        );
        assert!(snapshot.source_control.hosts["git.example.com"].enabled);
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("github-writer-secret")
        );
    });
    state.dispatch_command(
        cx,
        641,
        Command::SetHostToken {
            host: "github.com".into(),
            token: None,
            email: None,
        },
    );
    cx.run_until(|_| persisted().get("@github").is_none());
    assert_eq!(
        settings_store.profile_secrets("github").unwrap()["ANTHROPIC_API_KEY"],
        "provider-replacement-secret"
    );
    state.dispatch_command(
        cx,
        643,
        Command::SetProfileSecret {
            profile_id: "github".into(),
            name: "ANTHROPIC_API_KEY".into(),
            value: None,
        },
    );
    cx.run_until(|_| settings_store.profile_secrets("github").unwrap().is_empty());
    let outgoing = cx.drain_outgoing();
    assert!(
        !serde_json::to_string(&outgoing)
            .unwrap()
            .contains("github-writer-secret")
    );
    assert!(
        !std::fs::read_to_string(store.root().join("settings.json"))
            .unwrap()
            .contains("github-writer-secret")
    );
    let unreadable = r#"{"version":1,"profiles":{"claude":{"KEY":"unread-secret"}}}"#;
    std::fs::write(store.root().join("secrets.json"), unreadable).unwrap();
    state.dispatch_command(
        cx,
        644,
        Command::SetHostToken {
            host: "github.com".into(),
            token: Some("replacement".into()),
            email: None,
        },
    );
    cx.run_until_sent(|message| {
        matches!(message, HostMessage::Ack { id: 644, result: Err(error) } if error.code == "settings_write_failed")
            .then_some(())
    });
    assert_eq!(
        std::fs::read_to_string(store.root().join("secrets.json")).unwrap(),
        unreadable
    );
}
