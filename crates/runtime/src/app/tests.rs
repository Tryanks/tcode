use super::test_support::*;
use super::*;
use super::{active_session::*, events::*, orchestrate::*, providers::*};

use tcode_core::project::group_sessions;
use tcode_core::settings::{SettingsPatch, ThemeMode};
use tcode_protocol::{Command, CommandResponse, HostMessage};

#[test]
fn denied_screen_recording_drops_permission_relaunch_marker() {
    let marker = tcode_services::relaunch::RelaunchMarker {
        reopen_settings: "computer_use".into(),
        active_session: Some("session-1".into()),
    };

    assert_eq!(
        permission_relaunch_marker(
            Some(marker),
            computer_use_mcp::permissions::PermissionStatus::default(),
        ),
        None
    );
}

#[test]
fn provider_native_subagent_events_create_and_feed_read_only_mirror_session() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-native-subagent-mirror-test");
    let state = cx.new_entity(|_| AppState::new((*test_store).clone()));

    state.update(cx, |state, cx| {
        let mut parent_meta = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/native-subagent-parent"),
            Some("gpt-test".into()),
        );
        parent_meta.id = "parent".into();
        parent_meta.project_id = Some("project".into());
        state.sessions.push(parent_meta.clone());
        state.residents.active = Some(ActiveSession::new(parent_meta, false, Vec::new()));

        state.on_event(
            "parent",
            AgentEvent::ItemStarted(ThreadItem {
                id: "spawn-1".into(),
                parent_item_id: None,
                content: ItemContent::Subagent {
                    agent_type: "explorer".into(),
                    description: "Inspect event routing\nand report".into(),
                    status: ItemStatus::InProgress,
                    summary: None,
                },
            }),
            cx,
        );

        let mirror = state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.as_deref() == Some("spawn-1"))
            .cloned()
            .expect("native subagent mirror metadata");
        assert_eq!(mirror.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(mirror.title, "explorer: Inspect event routing");
        assert!(state.resident(&mirror.id).unwrap().has_work());
        assert!(state.resident(&mirror.id).unwrap().timeline.turn_running);

        state.on_event(
            "parent",
            AgentEvent::ItemCompleted(ThreadItem {
                id: "child-answer".into(),
                parent_item_id: Some("spawn-1".into()),
                content: ItemContent::AssistantMessage {
                    text: "routed transcript".into(),
                },
            }),
            cx,
        );
        let mirror_timeline = &state.resident(&mirror.id).unwrap().timeline;
        assert!(mirror_timeline.entries.iter().any(|entry| {
            matches!(
                &entry.content,
                EntryContent::Item(ItemContent::AssistantMessage { text })
                    if entry.id == "child-answer" && text == "routed transcript"
            )
        }));
        assert_eq!(state.resident("parent").unwrap().timeline.entries.len(), 1);

        state.on_event(
            "parent",
            AgentEvent::ItemCompleted(ThreadItem {
                id: "spawn-1".into(),
                parent_item_id: None,
                content: ItemContent::Subagent {
                    agent_type: "explorer".into(),
                    description: "Inspect event routing\nand report".into(),
                    status: ItemStatus::Completed,
                    summary: Some("routing verified".into()),
                },
            }),
            cx,
        );
        assert!(!state.resident(&mirror.id).unwrap().has_work());
        assert!(!state.resident(&mirror.id).unwrap().timeline.turn_running);
        assert!(matches!(
            &state.resident("parent").unwrap().timeline.entries[0].content,
            EntryContent::Item(ItemContent::Subagent {
                status: ItemStatus::Completed,
                summary: Some(summary),
                ..
            }) if summary == "routing verified"
        ));

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_status("parent".into(), None, reply, cx);
        assert_eq!(response.try_recv().unwrap().unwrap(), serde_json::json!([]));
    });
    cx.run_until_parked();

    state.update(cx, |state, _| {
        let mirror = state
            .sessions
            .iter()
            .find(|meta| meta.native_subagent.as_deref() == Some("spawn-1"))
            .unwrap();
        let mirror_events = state.store.read_events(&mirror.id);
        assert!(mirror_events.iter().any(|stored| matches!(
            &stored.event,
            AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                ..
            }) if id == "child-answer"
        )));
        let parent_events = state.store.read_events("parent");
        assert!(parent_events.iter().any(|stored| matches!(
            &stored.event,
            AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::Subagent { .. },
            }) if id == "spawn-1"
        )));
        assert!(parent_events.iter().all(|stored| match &stored.event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => item.parent_item_id.is_none(),
            _ => true,
        }));
    });
}

#[test]
fn settings_patch_preserves_concurrently_changed_other_field() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-dispatch-settings-seam-test");
    let state = cx.new_entity(|_| AppState::new((*test_store).clone()));
    let settings = Settings {
        sidebar_collapsed: true,
        ..Settings::default()
    };

    state.update(cx, |state, cx| state.update_settings(settings.clone(), cx));
    cx.run_until_parked();
    cx.drain_outgoing();

    state.dispatch_command(
        cx,
        41,
        Command::PatchSettings {
            patch: tcode_protocol::SettingsPatch::ThemeMode(ThemeMode::Dark),
        },
    );
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Ack {
            id: 41,
            result: Ok(CommandResponse::Unit)
        }
    )));
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            topic: Topic::Settings,
            event: ServerEvent::SettingsReplaced(replaced),
            ..
        }) if replaced.sidebar_collapsed && replaced.theme_mode == ThemeMode::Dark
    )));
}

#[test]
fn settings_patches_from_stale_snapshot_preserve_nested_sibling_fields() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-dispatch-nested-settings-seam-test");
    let state = cx.new_entity(|_| AppState::new((*test_store).clone()));
    let stale = Settings::default().browser;
    let mut home_url_writer = stale.clone();
    home_url_writer.home_url = Some("https://example.com".into());
    let mut allow_evaluate_writer = stale;
    allow_evaluate_writer.allow_evaluate = false;
    let home_url_patch = SettingsPatch::BrowserHomeUrl(home_url_writer.home_url);
    let allow_evaluate_patch =
        SettingsPatch::BrowserAllowEvaluate(allow_evaluate_writer.allow_evaluate);

    state.dispatch_command(
        cx,
        42,
        Command::PatchSettings {
            patch: home_url_patch,
        },
    );
    state.dispatch_command(
        cx,
        43,
        Command::PatchSettings {
            patch: allow_evaluate_patch,
        },
    );
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            topic: Topic::Settings,
            event: ServerEvent::SettingsReplaced(replaced),
            ..
        }) if replaced.browser.home_url.as_deref() == Some("https://example.com")
            && !replaced.browser.allow_evaluate
    )));
}

#[test]
fn reset_settings_clears_preferences_but_keeps_credentials_installs_and_unknown_keys() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-reset-settings-scope-test");
    let state = cx.new_entity(|_| AppState::new((*test_store).clone()));

    // Preferences the reset must clear.
    let mut settings = Settings {
        theme_mode: ThemeMode::Dark,
        language: Some("zh-CN".into()),
        word_wrap_diffs: true,
        auto_archive_max_idle_days: 99,
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
        },
    );
    settings.collapsed_projects.push("project".into());
    settings.favorite_models.push("gpt-5.6-sol".into());
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
        assert_eq!(reset.auto_archive_max_idle_days, 7);
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
        assert_eq!(reset.favorite_models, vec!["gpt-5.6-sol".to_string()]);
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

#[test]
fn provider_projection_diff_emits_once_then_suppresses_noop_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-provider-diff-test");
    let state = cx.new_entity(|_| AppState::new((*test_store).clone()));
    let command = Command::SetProfileSecret {
        profile_id: "codex".into(),
        name: "OPENAI_API_KEY".into(),
        value: Some("test-secret".into()),
    };

    state.dispatch_command(cx, 43, command.clone());
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    let provider_events: Vec<_> = outgoing
        .iter()
        .filter_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                topic: Topic::Providers,
                event: ServerEvent::ProvidersReplaced(status),
            }) => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(provider_events.len(), 1);
    assert!(
        provider_events[0]
            .secret_names
            .get("codex")
            .is_some_and(|names| names.contains("OPENAI_API_KEY"))
    );

    state.dispatch_command(cx, 44, command);
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    assert!(!outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            topic: Topic::Providers,
            event: ServerEvent::ProvidersReplaced(_),
        })
    )));
}

#[test]
fn parked_session_projection_diff_emits_session_status() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-parked-session-diff-test");
    let state = cx.new_entity(|_| AppState::new((*test_store).clone()));
    let parked = live_session(ProviderKind::Codex, smol::channel::unbounded().0);
    let parked_id = parked.meta.id.clone();

    state.host_update(cx, |state, _cx| {
        state.sessions.push(parked.meta.clone());
        state
            .residents
            .parked
            .insert(parked.meta.id.clone(), parked);
    });
    cx.run_until_parked();
    cx.drain_outgoing();

    state.dispatch_command(
        cx,
        45,
        Command::RenameSession {
            session_id: parked_id.clone(),
            title: "Renamed while parked".into(),
        },
    );
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    let statuses: Vec<_> = outgoing
        .iter()
        .filter_map(|message| match message {
            HostMessage::Event(EventEnvelope {
                topic: Topic::SessionStatus { session_id },
                event: ServerEvent::SessionStatusReplaced(status),
            }) if session_id == &parked_id => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].title, "Renamed while parked");
    state.read_with(cx, |state, _| {
        assert!(state.residents.active.is_none());
        assert!(state.residents.parked.contains_key(&parked_id));
    });
}

#[test]
fn dispatched_start_draft_emits_session_status_over_ndjson() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-dispatch-session-status-seam-test");
    let state = cx.new_entity(|_| AppState::new((*test_store).clone()));
    let cwd = test_store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    state.dispatch_command(
        cx,
        42,
        Command::StartDraft {
            project_id: "project-1".into(),
            cwd: cwd.clone(),
        },
    );
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            topic: Topic::SessionStatus { session_id },
            event: ServerEvent::SessionStatusReplaced(status),
            ..
        }) if session_id == &status.session_id
            && status.project_id.as_deref() == Some("project-1")
            && status.cwd == cwd
            && status.draft
    )));
}

#[test]
fn scripted_provider_connects_command_launch_and_agent_event_paths() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scripted-provider-seam-test");
    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let commands = scripted.commands.clone();
    let events = scripted.events.clone();
    let state = cx.new_entity(|_| {
        let mut state = AppState::new((*test_store).clone());
        state.set_provider_launcher_for_test(scripted.launcher);
        state
    });
    let cwd = test_store.root().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    state.dispatch_command(
        cx,
        51,
        Command::StartDraft {
            project_id: "project-1".into(),
            cwd,
        },
    );
    state.dispatch_command(
        cx,
        52,
        Command::SendTurn {
            text: "exercise the adapter".into(),
            attachment_paths: Vec::new(),
        },
    );
    cx.run_until_parked();

    let delivery_id = match commands.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) => {
            assert_eq!(text, "exercise the adapter");
            delivery_id
        }
        other => panic!("expected scripted provider SendTurn, got {other:?}"),
    };
    events
        .try_send(AgentEvent::TurnAccepted { delivery_id })
        .unwrap();
    events
        .try_send(AgentEvent::TurnStarted {
            turn_id: "scripted-turn".into(),
        })
        .unwrap();
    cx.run_until_parked();

    let outgoing = cx.drain_outgoing();
    assert!(outgoing.iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            topic: Topic::SessionEvents { .. },
            event: ServerEvent::SessionEvent(SessionEventRecord {
                event: AgentEvent::TurnStarted { turn_id },
                ..
            }),
            ..
        }) if turn_id == "scripted-turn"
    )));
    state.read_with(cx, |state, _| {
        assert!(state.residents.active.as_ref().unwrap().turn_in_flight);
    });
}

#[test]
fn archive_and_unarchive_apply_exact_timestamp_cascades() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-archive-cascade-test");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    for (id, parent) in [
        ("parent", None),
        ("child", Some("parent")),
        ("grandchild", Some("child")),
    ] {
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_string);
        store.upsert_meta(&meta).unwrap();
    }
    let state = cx.new_entity(|_| AppState::new(store.clone()));

    state.host_update(cx, |state, cx| {
        state.archive_session("parent", cx);
        let archived_at = state
            .sessions
            .iter()
            .find(|meta| meta.id == "parent")
            .unwrap()
            .archived_at
            .unwrap();
        assert!(
            state
                .sessions
                .iter()
                .all(|meta| meta.archived_at == Some(archived_at))
        );

        let grandchild = state
            .sessions
            .iter_mut()
            .find(|meta| meta.id == "grandchild")
            .unwrap();
        grandchild.archived_at = Some(archived_at + 1);
        let grandchild = grandchild.clone();
        state.persist_meta(&grandchild, cx);

        state.unarchive_session("parent", cx);
        assert_eq!(
            state
                .sessions
                .iter()
                .find(|meta| meta.id == "parent")
                .unwrap()
                .archived_at,
            None
        );
        assert_eq!(
            state
                .sessions
                .iter()
                .find(|meta| meta.id == "child")
                .unwrap()
                .archived_at,
            None
        );
        assert_eq!(
            state
                .sessions
                .iter()
                .find(|meta| meta.id == "grandchild")
                .unwrap()
                .archived_at,
            Some(archived_at + 1)
        );
    });
    cx.run_until_parked();
    let persisted = store.load_index();
    assert!(
        persisted
            .iter()
            .find(|meta| meta.id == "grandchild")
            .unwrap()
            .archived_at
            .is_some()
    );
    assert!(
        persisted
            .iter()
            .filter(|meta| meta.id != "grandchild")
            .all(|meta| meta.archived_at.is_none())
    );
}

#[test]
fn generated_titles_are_cleaned_and_bounded() {
    assert_eq!(
        sanitize_generated_title("  **Title: Fix sidebar rename.**  ").as_deref(),
        Some("Fix sidebar rename")
    );
    assert_eq!(
        sanitize_generated_title("# 「标题：为对话生成简洁标题。」").as_deref(),
        Some("为对话生成简洁标题")
    );
    assert_eq!(sanitize_generated_title("  ` `  "), None);

    let long = "a".repeat(TITLE_MAX_CHARS + 10);
    let title = sanitize_generated_title(&long).unwrap();
    assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
    assert!(title.ends_with('…'));
}

#[test]
fn provider_diagnostics_with_zero_output_tokens_are_not_titles() {
    let mut usage = agent::TokenUsage {
        output_tokens: Some(0),
        ..Default::default()
    };
    assert!(!title_turn_generated_output(
        TurnStatus::Completed,
        Some(&usage)
    ));

    usage.output_tokens = Some(4);
    assert!(title_turn_generated_output(
        TurnStatus::Completed,
        Some(&usage)
    ));
    assert!(title_turn_generated_output(TurnStatus::Completed, None));
    assert!(!title_turn_generated_output(TurnStatus::Failed, None));
}

#[test]
fn title_prompt_treats_the_request_as_bounded_json_data() {
    let escaped = title_generation_prompt("line one\nline two", true);
    assert!(escaped.contains("untrusted source text"));
    assert!(escaped.contains("original image attachments"));
    assert!(escaped.contains("\\n"), "the request is JSON escaped");

    let source = "界".repeat(TITLE_SOURCE_MAX_CHARS + 20);
    let prompt = title_generation_prompt(&source, false);
    assert!(prompt.contains("untrusted source text"));
    assert!(!prompt.contains(&"界".repeat(TITLE_SOURCE_MAX_CHARS + 1)));
    assert!(prompt.contains(&format!("{}…", "界".repeat(TITLE_SOURCE_MAX_CHARS))));
}

#[test]
fn title_session_uses_configured_model_with_low_effort() {
    let defaults = title_session_meta(&Settings::default(), PathBuf::from("/tmp/project"));
    assert_eq!(defaults.provider, ProviderKind::Codex);
    assert_eq!(defaults.model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(
        defaults.option_selections,
        vec![OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("low"),
        }]
    );

    let mut settings = Settings::default();
    settings.title_generation.provider = ProviderKind::ClaudeCode;
    settings.title_generation.model = "claude-haiku-4-5".into();
    settings.profiles.insert(
        "work-claude".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings::default(),
        },
    );
    settings.title_generation.profile_id = Some("work-claude".into());
    let custom = title_session_meta(&settings, PathBuf::from("/tmp/project"));
    assert_eq!(custom.provider, ProviderKind::ClaudeCode);
    assert_eq!(custom.model.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(custom.profile_id.as_deref(), Some("work-claude"));
    assert_eq!(custom.approval_mode, ApprovalMode::Supervised);
    assert_eq!(custom.interaction_mode, InteractionMode::Build);
    assert!(!custom.orchestrate_enabled);
    assert_eq!(
        title_turn_options().effort.as_deref(),
        Some(AI_TITLE_REASONING_EFFORT)
    );

    settings.title_generation.profile_id = Some("deleted-profile".into());
    let fallback = title_session_meta(&settings, PathBuf::from("/tmp/project"));
    assert_eq!(fallback.profile_id, None);
}

#[test]
fn parse_fallback_review_with_delimiter() {
    assert_eq!(
        parse_fallback_review(
            "ASSESSMENT: This looks legitimate. The scope is specific.\n---DRAFT---\nI own the test system."
        ),
        (
            "This looks legitimate. The scope is specific.".into(),
            "I own the test system.".into()
        )
    );
}

#[test]
fn parse_fallback_review_without_delimiter() {
    assert_eq!(
        parse_fallback_review("A cautious assessment without the expected separator."),
        (
            "A cautious assessment without the expected separator.".into(),
            String::new()
        )
    );
}

#[test]
fn parse_fallback_review_with_empty_draft() {
    assert_eq!(
        parse_fallback_review("ASSESSMENT: This appears genuinely concerning.\n---DRAFT---\n"),
        ("This appears genuinely concerning.".into(), String::new())
    );
}

#[test]
fn parse_fallback_review_strips_case_insensitive_label_with_whitespace() {
    assert_eq!(
        parse_fallback_review(
            "  assessment   :   Likely benign.\n  ---DRAFT---  \n  I administer this host.  "
        ),
        ("Likely benign.".into(), "I administer this host.".into())
    );
}

#[test]
fn late_ai_title_does_not_overwrite_a_manual_rename() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-ai-title-race-test");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
    meta.title = "first message fallback".into();
    let id = meta.id.clone();
    store.upsert_meta(&meta).unwrap();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        state.apply_generated_title(&id, "first message fallback", "AI generated title", cx);
        assert_eq!(state.sessions[0].title, "AI generated title");

        state.rename_session(&id, "My manual title", cx);
        state.apply_generated_title(&id, "AI generated title", "Late replacement", cx);
        assert_eq!(state.sessions[0].title, "My manual title");
    });
}

#[test]
fn marketplace_items_are_runtime_owned_views() {
    let test_store = TestStore::new("tcode-marketplace-view-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);
    state.acp_registry = Some(
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
        },
    );
    state.acp_installing.insert("last".into());

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

#[test]
fn provider_update_command_hides_install_source() {
    let test_store = TestStore::new("tcode-provider-update-view-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);
    state.providers.provider_versions.insert(
        ProviderKind::ClaudeCode,
        ProviderVersionState {
            install_source: InstallSource::Npm,
            ..ProviderVersionState::default()
        },
    );
    state.providers.provider_versions.insert(
        ProviderKind::Codex,
        ProviderVersionState {
            install_source: InstallSource::Native,
            ..ProviderVersionState::default()
        },
    );

    assert_eq!(
        state.provider_update_command(ProviderKind::ClaudeCode),
        Some("npm install -g @anthropic-ai/claude-code@latest".into())
    );
    assert_eq!(state.provider_update_command(ProviderKind::Codex), None);
    assert_eq!(state.provider_update_command(ProviderKind::Acp), None);
}

#[test]
fn orchestrate_guidance_and_current_configuration_are_composed() {
    let settings = OrchestrateSettings {
        generic_identity: "Generic lead".into(),
        ..Default::default()
    };
    let first = compose_orchestrate_text(
        ProviderKind::ClaudeCode,
        Some("claude-fable-5"),
        true,
        &settings,
        "Ship it",
    );
    assert!(first.starts_with(FABLE_ORCHESTRATE_GUIDANCE.trim()));
    assert!(first.contains("wise owl"));
    assert!(first.contains("#### `codex` / `gpt-5.6-sol` — effort `medium`"));
    assert!(first.contains("cost efficiency 9, intelligence 8, taste 6"));
    assert!(first.contains("#### `claude` / `claude-opus-4-8` — effort `high`"));
    assert!(first.ends_with("\n\nShip it"));
    let follow_up = compose_orchestrate_text(
        ProviderKind::ClaudeCode,
        Some("claude-opus-4-8"),
        false,
        &settings,
        "Follow up",
    );
    assert!(!follow_up.contains(FABLE_ORCHESTRATE_GUIDANCE));
    assert!(follow_up.starts_with("## Current orchestrator configuration"));
    assert!(follow_up.contains("Generic lead"));
    assert!(follow_up.ends_with("\n\nFollow up"));

    let codex = compose_orchestrate_text(ProviderKind::Codex, None, true, &settings, "Implement");
    assert!(codex.starts_with(CODEX_ORCHESTRATE_GUIDANCE.trim()));
    assert!(codex.ends_with("\n\nImplement"));
    assert!(codex.contains("Generic lead"));

    let acp = compose_orchestrate_text(
        ProviderKind::Acp,
        Some("gemini-3-pro"),
        true,
        &settings,
        "Coordinate",
    );
    assert!(acp.starts_with(GENERIC_ORCHESTRATE_GUIDANCE.trim()));
    assert!(acp.contains("Generic lead"));
}

#[test]
fn send_turn_assembles_draft_context_and_attachment_paths() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-send-assembly-test");
    let root = test_store.root().clone();
    std::fs::create_dir_all(&root).unwrap();
    let attachment_path = root.join("sample.png");
    std::fs::write(&attachment_path, [1, 2, 3]).unwrap();
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "assembled".into();
        active.terminal_workspace.contexts.push(TerminalContext {
            id: 1,
            terminal_label: "zsh".into(),
            line_start: 12,
            line_end: 13,
            text: "cargo test\nok".into(),
        });
        state.residents.active = Some(active);
        state.add_review_comment(
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
            cx,
        );

        state.send_turn("Explain this".into(), vec![attachment_path.clone()], cx);

        let SessionCommand::SendTurn {
            text, attachments, ..
        } = receiver.try_recv().expect("assembled send command")
        else {
            panic!("expected SendTurn")
        };
        assert_eq!(
            text,
            "Explain this\n\n<terminal_context>\n- zsh lines 12-13:\n  12 | cargo test\n  13 | ok\n</terminal_context>\n\n<review_comment sectionId=\"section\" sectionTitle=\"Changes\" filePath=\"src/lib.rs\" startIndex=\"3\" endIndex=\"4\" rangeLabel=\"+7\">\nPlease fix\n```diff\nlet bad = true;\n```\n</review_comment>"
        );
        assert_eq!(
            attachments,
            vec![Attachment {
                media_type: "image/png".into(),
                data_base64: "AQID".into(),
                source_path: Some(attachment_path.to_string_lossy().into_owned()),
            }]
        );
        assert!(
            state
                .residents
                .active
                .as_ref()
                .unwrap()
                .terminal_workspace
                .contexts
                .is_empty()
        );
        assert!(state.review_comments().is_empty());
    });
}

#[test]
fn orchestrate_turn_records_the_context_split_on_the_user_message() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-split-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();
    let mut expected_full = String::new();
    let mut expected_context = 0;

    state.host_update(cx, |state, cx| {
        // A live, idle, already-enabled orchestrator: the turn is an ordinary
        // send (no restart, nothing in flight), so it flows through
        // record_user_message where the split is stored.
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "orchestrator".into();
        active.meta.orchestrate_enabled = true;
        // Match the live launch state so the send is an ordinary turn rather
        // than a restart (which would flush through a different path).
        active.live_model = active.meta.model.clone();
        active.live_approval_mode = Some(active.meta.approval_mode);
        state.residents.active = Some(active);

        state.orchestrate_turn("执行某某任务".into(), Vec::new(), cx);
        let delivery_id = match receiver.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected orchestrator SendTurn, got {other:?}"),
        };
        state.on_event("orchestrator", AgentEvent::TurnAccepted { delivery_id }, cx);

        // What the provider actually receives is the whole composed text.
        expected_full = compose_orchestrate_text(
            ProviderKind::Codex,
            None,
            false,
            &state.settings.orchestrate,
            "执行某某任务",
        );
        expected_context = expected_full.len() - "执行某某任务".len();
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let events = state.store.read_events("orchestrator");
        let recorded = events
            .iter()
            .find_map(|stored| match &stored.event {
                AgentEvent::ItemCompleted(ThreadItem {
                    content:
                        ItemContent::UserMessage {
                            text, context_len, ..
                        },
                    ..
                }) => Some((text.clone(), *context_len)),
                _ => None,
            })
            .expect("orchestrate turn recorded a user message");
        assert_eq!(recorded.0, expected_full);
        assert_eq!(recorded.1, Some(expected_context));

        // Folded, the timeline splits the prefix from the user's own words.
        let timeline = Timeline::fold_events(events);
        let user = timeline
            .entries
            .iter()
            .find_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::UserMessage {
                    text,
                    context_len: Some(len),
                    ..
                }) => Some((text.clone(), *len)),
                _ => None,
            })
            .expect("folded user entry carries the split");
        assert_eq!(&user.0[user.1..], "执行某某任务");
    });
}

#[test]
fn orchestrate_title_generation_uses_only_the_users_request() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-title-source-test");
    let scripted_title = scripted_provider(ProviderKind::Codex);
    let title_commands = scripted_title.commands.clone();
    let title_events = scripted_title.events.clone();
    let state = cx.new_entity(|_| {
        let mut state = AppState::new((*test_store).clone());
        state.ai_title_generation_enabled = true;
        state.set_provider_launcher_for_test(scripted_title.launcher);
        state
    });
    let (commands, receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "orchestrator-title".into();
        active.meta.orchestrate_enabled = true;
        active.live_model = active.meta.model.clone();
        active.live_approval_mode = Some(active.meta.approval_mode);
        state.residents.active = Some(active);

        state.orchestrate_turn("执行某某任务".into(), Vec::new(), cx);
        let delivery_id = match receiver.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected orchestrator SendTurn, got {other:?}"),
        };
        state.on_event(
            "orchestrator-title",
            AgentEvent::TurnAccepted { delivery_id },
            cx,
        );
    });
    cx.run_until_parked();

    let title_prompt = match title_commands.try_recv() {
        Ok(SessionCommand::SendTurn { text, .. }) => text,
        other => panic!("expected title-generation SendTurn, got {other:?}"),
    };
    assert_eq!(
        title_prompt,
        title_generation_prompt("执行某某任务", false),
        "the hidden orchestrate prefix must not be sent to the title model"
    );

    title_events
        .try_send(AgentEvent::ItemCompleted(ThreadItem {
            id: "title".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "执行某某任务".into(),
            },
        }))
        .unwrap();
    title_events
        .try_send(AgentEvent::TurnCompleted {
            turn_id: "title-turn".into(),
            status: TurnStatus::Completed,
            usage: None,
        })
        .unwrap();
    title_events
        .try_send(AgentEvent::SessionClosed { reason: None })
        .unwrap();
    cx.run_until_parked();
}

#[test]
fn orchestrate_dispatch_enforces_child_allow_list_and_defaults() {
    let mut settings = OrchestrateSettings::default();
    let mut custom_profile = settings.child_models[0].clone();
    custom_profile.profile_id = Some("kimi".into());
    settings.child_models.push(custom_profile);
    assert_eq!(
        resolve_orchestrate_dispatch(&settings, "codex", None, None, None).unwrap(),
        (
            ProviderKind::Codex,
            "gpt-5.6-sol".into(),
            Some("medium".into()),
            None
        )
    );
    assert_eq!(
        resolve_orchestrate_dispatch(
            &settings,
            "codex",
            Some("gpt-5.6-sol"),
            Some("medium"),
            Some("KIMI"),
        )
        .unwrap(),
        (
            ProviderKind::Codex,
            "gpt-5.6-sol".into(),
            Some("medium".into()),
            Some("kimi".into()),
        )
    );
    let unknown_profile = resolve_orchestrate_dispatch(
        &settings,
        "codex",
        Some("gpt-5.6-sol"),
        Some("medium"),
        Some("missing"),
    )
    .unwrap_err();
    assert!(unknown_profile.contains("profile missing"));
    assert!(unknown_profile.contains("profile kimi"));
    assert_eq!(
        resolve_orchestrate_dispatch(
            &settings,
            "claude_code",
            Some("claude-opus-4-8"),
            Some(" HIGH "),
            None,
        )
        .unwrap(),
        (
            ProviderKind::ClaudeCode,
            "claude-opus-4-8".into(),
            Some("high".into()),
            None
        )
    );
    let wrong_effort =
        resolve_orchestrate_dispatch(&settings, "codex", Some("gpt-5.6-sol"), Some("xhigh"), None)
            .unwrap_err();
    assert!(
        wrong_effort
            .contains("no enabled child profile matches gpt-5.6-sol (effort xhigh) under codex")
    );
    assert!(wrong_effort.contains("gpt-5.6-sol (effort medium)"));
    assert!(wrong_effort.contains("gpt-5.6-sol (effort max)"));
    let denied =
        resolve_orchestrate_dispatch(&settings, "claude", Some("claude-haiku-4-5"), None, None)
            .unwrap_err();
    assert!(denied.contains("no enabled child profile matches"));

    let mut empty = settings;
    empty.child_models.clear();
    assert!(
        resolve_orchestrate_dispatch(&empty, "codex", None, None, None)
            .unwrap_err()
            .contains("enabled profiles: none")
    );
}

#[test]
fn orchestrate_dispatch_access_maps_known_values() {
    assert_eq!(resolve_dispatch_access(None), Ok(ApprovalMode::FullAccess));
    assert_eq!(
        resolve_dispatch_access(Some(" FULL ")),
        Ok(ApprovalMode::FullAccess)
    );
    assert_eq!(
        resolve_dispatch_access(Some("read_only")),
        Ok(ApprovalMode::ReadOnly)
    );
    assert_eq!(
        resolve_dispatch_access(Some("WORKSPACE_WRITE")),
        Ok(ApprovalMode::AutoAcceptEdits)
    );
    assert_eq!(
        resolve_dispatch_access(Some("admin")),
        Err("unknown access: admin; expected read_only, workspace_write, or full".into())
    );
}

#[test]
fn updates_on_the_viewed_thread_do_not_mark_it_unread() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-viewed-unread-test");
    let store = (*test_store).clone();
    let first = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/a"), None);
    let second = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/b"), None);
    store.upsert_meta(&first).unwrap();
    store.upsert_meta(&second).unwrap();
    let first_id = first.id.clone();
    let second_id = second.id.clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        state.select_session(&first_id, cx);

        // A turn finishes while the user is watching: updated_at moves past
        // the watermark stamped on entry, and the meta is persisted.
        let active = state.residents.active.as_mut().unwrap();
        active.meta.updated_at = now_secs() + 10;
        let meta = active.meta.clone();
        state.persist_meta(&meta, cx);

        // Switching away must not surface an unread dot for what the user
        // already saw happen on screen.
        state.select_session(&second_id, cx);
        assert!(!state.session_unread(&first_id));

        // But an update landing on a thread the user is NOT viewing still
        // marks it unread.
        let mut parked = state
            .sessions
            .iter()
            .find(|m| m.id == first_id)
            .cloned()
            .unwrap();
        parked.updated_at = now_secs() + 20;
        state.persist_meta(&parked, cx);
        assert!(state.session_unread(&first_id));
    });
}

#[test]
fn draft_send_creates_session_with_project_cwd() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-draft-test");
    let store = (*test_store).clone();
    let project = Project::from_root(PathBuf::from("/tmp/tcode-draft-proj"));
    // Persist the project so the draft's project_id survives index migration.
    store.upsert_project(&project).unwrap();
    let state = cx.new_entity(|_| AppState::new(store));
    // A draft is set up (cwd = project root) but not yet persisted.
    let draft = AppState::build_draft_session(
        project.id.clone(),
        project.root.clone(),
        ProviderKind::ClaudeCode,
        None,
        None,
        Vec::new(),
    );
    assert!(draft.draft);
    assert_eq!(draft.meta.cwd, project.root);
    assert_eq!(draft.meta.project_id.as_deref(), Some(project.id.as_str()));
    assert!(matches!(draft.runtime, Runtime::Idle));
    let draft_id = draft.meta.id.clone();
    state.host_update(cx, |state, cx| {
        state.residents.active = Some(draft);
        // Not in the index until the first send materializes it.
        assert!(!state.sessions.iter().any(|m| m.id == draft_id));

        // The first send commits the draft: it becomes a real session whose
        // cwd is the project root and shows up in the sidebar index.
        state.commit_draft(cx).unwrap();
        assert!(!state.residents.active.as_ref().unwrap().draft);
        let created = state.sessions.iter().find(|m| m.id == draft_id).unwrap();
        assert_eq!(created.cwd, project.root);
        assert_eq!(created.project_id.as_deref(), Some(project.id.as_str()));
    });
}

#[test]
fn draft_inherits_newest_unarchived_session_from_same_project() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-draft-project-defaults-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        let mut other_project = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/other"),
            Some("opus".into()),
        );
        other_project.project_id = Some("project-other".into());
        other_project.updated_at = 900;
        other_project.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("minimal"),
        });

        let mut target_older = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/target-old"),
            Some("sonnet".into()),
        );
        target_older.project_id = Some("project-target".into());
        target_older.updated_at = 100;
        target_older.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("medium"),
        });

        let mut target_newest = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/target-new"),
            Some("gpt-5.2-codex".into()),
        );
        target_newest.project_id = Some("project-target".into());
        target_newest.updated_at = 500;
        target_newest.option_selections = vec![
            OptionSelection {
                id: "serviceTier".into(),
                value: serde_json::json!("fast"),
            },
            OptionSelection {
                id: "reasoningEffort".into(),
                value: serde_json::json!("high"),
            },
        ];

        let mut target_archived = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/target-archived"),
            Some("haiku".into()),
        );
        target_archived.project_id = Some("project-target".into());
        target_archived.updated_at = 800;
        target_archived.archived_at = Some(801);
        target_archived.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("low"),
        });

        // Deliberately interleaved and not timestamp-sorted: selection must
        // be project-scoped and based on updated_at, not vector position.
        state.sessions = vec![other_project, target_older, target_archived, target_newest];
        state.start_draft("project-target".into(), PathBuf::from("/tmp/target"), cx);

        let draft = state.residents.active.as_ref().unwrap();
        assert!(draft.draft);
        assert_eq!(draft.meta.provider, ProviderKind::Codex);
        assert_eq!(draft.meta.model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(draft.meta.acp_agent_id, None);
        assert_eq!(draft.meta.option_selections.len(), 1);
        assert_eq!(draft.meta.option_selections[0].id, "reasoningEffort");
        assert_eq!(
            draft.meta.option_selections[0].value,
            serde_json::json!("high")
        );
        assert!(state.store.load_index().is_empty());
    });
}

#[test]
fn draft_model_selection_switches_to_the_rows_explicit_provider() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-draft-provider-selection-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
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
        state.sessions = vec![previous];
        state.start_draft("project-target".into(), PathBuf::from("/tmp/target"), cx);

        let draft = state.residents.active.as_ref().unwrap();
        assert_eq!(draft.meta.provider, ProviderKind::Codex);
        assert_eq!(draft.meta.model.as_deref(), Some("gpt-5.6-sol"));

        // `claude-fable-5` cannot be reliably classified by a hard-coded
        // model-name heuristic. The provider comes from its picker row.
        state.set_active_model(
            ProviderKind::ClaudeCode,
            Some("claude-fable-5".into()),
            None,
            cx,
        );

        let draft = state.residents.active.as_ref().unwrap();
        assert_eq!(draft.meta.provider, ProviderKind::ClaudeCode);
        assert_eq!(draft.meta.model.as_deref(), Some("claude-fable-5"));
        assert!(draft.meta.acp_agent_id.is_none());
        assert!(draft.meta.option_selections.is_empty());
        assert!(state.store.load_index().is_empty());
    });
}

#[test]
fn model_switch_restores_last_effort_used_with_that_model() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-model-switch-effort-memory-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
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
            Some("claude-fable-5".into()),
        );
        fable.project_id = Some("project-target".into());
        fable.updated_at = 20;
        state.sessions = vec![sol, fable];
        state.start_draft("project-target".into(), PathBuf::from("/tmp/target"), cx);

        // Switching the draft to a model brings back the effort it last ran at,
        // not the model's default.
        state.set_active_model(ProviderKind::Codex, Some("gpt-5.6-sol".into()), None, cx);

        let draft = state.residents.active.as_ref().unwrap();
        assert_eq!(draft.meta.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(draft.meta.option_selections.len(), 1);
        assert_eq!(draft.meta.option_selections[0].id, "reasoningEffort");
        assert_eq!(
            draft.meta.option_selections[0].value,
            serde_json::json!("max")
        );
    });
}

#[test]
fn draft_inherits_acp_agent_id_from_project_history() {
    let test_store = TestStore::new("tcode-draft-acp-defaults-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);
    let mut acp = SessionMeta::new(
        ProviderKind::Acp,
        PathBuf::from("/tmp/acp"),
        Some("agent-model".into()),
    );
    acp.project_id = Some("project-acp".into());
    acp.acp_agent_id = Some("agent.example".into());
    acp.updated_at = 40;
    state.sessions = vec![acp];

    let (provider, model, acp_agent_id, _profile, effort) = state.draft_defaults("project-acp");
    assert_eq!(provider, ProviderKind::Acp);
    assert_eq!(model.as_deref(), Some("agent-model"));
    assert_eq!(acp_agent_id.as_deref(), Some("agent.example"));
    assert!(effort.is_none());
}

#[test]
fn draft_without_project_history_keeps_global_fallback_and_stays_unpersisted() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-draft-fallback-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        let mut global = SessionMeta::new(
            ProviderKind::Acp,
            PathBuf::from("/tmp/existing"),
            Some("fallback-model".into()),
        );
        global.project_id = Some("project-existing".into());
        global.acp_agent_id = Some("fallback-agent".into());
        global.updated_at = 200;
        global.option_selections.push(OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("low"),
        });
        state.sessions = vec![global];

        state.start_draft("project-empty".into(), PathBuf::from("/tmp/empty"), cx);

        let draft = state.residents.active.as_ref().unwrap();
        let draft_id = draft.meta.id.clone();
        assert!(draft.draft);
        assert_eq!(draft.meta.provider, ProviderKind::Acp);
        assert_eq!(draft.meta.model.as_deref(), Some("fallback-model"));
        assert_eq!(draft.meta.acp_agent_id.as_deref(), Some("fallback-agent"));
        assert!(draft.meta.option_selections.is_empty());
        assert!(
            !state
                .store
                .load_index()
                .iter()
                .any(|meta| meta.id == draft_id)
        );
    });
}

#[test]
fn draft_global_fallback_ignores_target_projects_archived_history() {
    let test_store = TestStore::new("tcode-draft-archived-fallback-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);

    let mut target_archived = SessionMeta::new(
        ProviderKind::Codex,
        PathBuf::from("/tmp/target-archived"),
        Some("gpt-5.2-codex".into()),
    );
    target_archived.project_id = Some("project-target".into());
    target_archived.updated_at = 900;
    target_archived.archived_at = Some(901);
    target_archived.option_selections.push(OptionSelection {
        id: "reasoningEffort".into(),
        value: serde_json::json!("high"),
    });

    let mut other_active = SessionMeta::new(
        ProviderKind::Acp,
        PathBuf::from("/tmp/other-active"),
        Some("active-model".into()),
    );
    other_active.project_id = Some("project-other".into());
    other_active.acp_agent_id = Some("active-agent".into());
    other_active.updated_at = 100;
    other_active.option_selections.push(OptionSelection {
        id: "reasoningEffort".into(),
        value: serde_json::json!("low"),
    });

    // The target's archived session is globally newest and first, but must
    // not be reselected by the global fallback.
    state.sessions = vec![target_archived, other_active];
    let (provider, model, acp_agent_id, _profile, effort) = state.draft_defaults("project-target");
    assert_eq!(provider, ProviderKind::Acp);
    assert_eq!(model.as_deref(), Some("active-model"));
    assert_eq!(acp_agent_id.as_deref(), Some("active-agent"));
    assert!(effort.is_none());
}

#[test]
fn draft_defaults_to_claude_when_all_sessions_are_archived() {
    let test_store = TestStore::new("tcode-draft-all-archived-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);

    let mut target_archived = SessionMeta::new(
        ProviderKind::Codex,
        PathBuf::from("/tmp/target-archived"),
        Some("gpt-5.2-codex".into()),
    );
    target_archived.project_id = Some("project-target".into());
    target_archived.updated_at = 200;
    target_archived.archived_at = Some(201);

    let mut other_archived = SessionMeta::new(
        ProviderKind::Acp,
        PathBuf::from("/tmp/other-archived"),
        Some("archived-model".into()),
    );
    other_archived.project_id = Some("project-other".into());
    other_archived.acp_agent_id = Some("archived-agent".into());
    other_archived.updated_at = 300;
    other_archived.archived_at = Some(301);

    state.sessions = vec![other_archived, target_archived];
    let (provider, model, acp_agent_id, _profile, effort) = state.draft_defaults("project-target");
    assert_eq!(provider, ProviderKind::ClaudeCode);
    assert!(model.is_none());
    assert!(acp_agent_id.is_none());
    assert!(effort.is_none());
}

/// A new draft must inherit the previous session's *profile*, not just its
/// model — otherwise "new thread" keeps the third-party model but routes it
/// to the built-in provider, which rejects it.
#[test]
fn draft_defaults_inherit_profile_id() {
    let test_store = TestStore::new("tcode-draft-profile-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);

    let mut prev = SessionMeta::new(
        ProviderKind::ClaudeCode,
        PathBuf::from("/tmp/kimi"),
        Some("k3[1m]".into()),
    );
    prev.project_id = Some("project-kimi".into());
    prev.profile_id = Some("klaude-kode".into());
    prev.updated_at = 500;
    state.sessions = vec![prev];

    let (provider, model, _acp, profile, _effort) = state.draft_defaults("project-kimi");
    assert_eq!(provider, ProviderKind::ClaudeCode);
    assert_eq!(model.as_deref(), Some("k3[1m]"));
    assert_eq!(
        profile.as_deref(),
        Some("klaude-kode"),
        "the draft must stay on the third-party profile"
    );
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
        .save_commands(ProviderKind::ClaudeCode, None, &commands)
        .unwrap();

    let state = AppState::new(SessionStore::open_at(root.clone()).unwrap());
    let seeded = state.cached_provider_commands(ProviderKind::ClaudeCode, None);
    let draft = AppState::build_draft_session(
        "project".into(),
        PathBuf::from("/tmp/project"),
        ProviderKind::ClaudeCode,
        None,
        None,
        seeded,
    );
    assert_eq!(draft.provider_commands, commands);
    assert!(matches!(draft.runtime, Runtime::Idle));
}

#[test]
fn configured_binary_reaches_session_options() {
    let codex = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
    let claude = SessionMeta::new(
        ProviderKind::ClaudeCode,
        PathBuf::from("/tmp/project"),
        None,
    );
    let mut settings = Settings::default();
    settings.provider_mut(ProviderKind::Codex).binary_path = Some(PathBuf::from("/custom/codex"));
    settings.provider_mut(ProviderKind::ClaudeCode).binary_path =
        Some(PathBuf::from("/custom/claude"));

    let codex_options = session_options(
        &codex,
        &settings,
        LaunchEnv::default(),
        None,
        None,
        None,
        None,
    );
    let claude_options = session_options(
        &claude,
        &settings,
        LaunchEnv::default(),
        None,
        None,
        None,
        None,
    );

    assert_eq!(
        codex_options.binary_path,
        Some(PathBuf::from("/custom/codex"))
    );
    assert_eq!(
        claude_options.binary_path,
        Some(PathBuf::from("/custom/claude"))
    );
    assert!(codex_options.mcp_servers.is_empty());
}

/// Settings → Providers env/home/launch-args must reach the spawn options,
/// and the home override must land on the provider's own variable.
#[test]
fn provider_env_home_and_launch_args_reach_session_options() {
    let mut settings = Settings::default();
    let claude = settings.provider_mut(ProviderKind::ClaudeCode);
    claude.home_path = Some(PathBuf::from("/tmp/claude-home"));
    claude.launch_args = Some("--chrome --verbose".into());
    let codex = settings.provider_mut(ProviderKind::Codex);
    codex.home_path = Some(PathBuf::from("/tmp/codex-shadow"));
    let pi = settings.provider_mut(ProviderKind::Pi);
    pi.launch_args = Some("--verbose".into());

    let launch_env = LaunchEnv {
        env: vec![("ANTHROPIC_BASE_URL".into(), "https://proxy.test".into())],
        home: settings
            .provider(ProviderKind::ClaudeCode)
            .home_path
            .clone(),
    };
    let meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
    let opts = session_options(&meta, &settings, launch_env, None, None, None, None);
    assert_eq!(opts.extra_args, vec!["--chrome", "--verbose"]);
    assert_eq!(
        opts.launch_env.pairs(ProviderKind::ClaudeCode),
        vec![
            (
                "ANTHROPIC_BASE_URL".to_string(),
                "https://proxy.test".to_string()
            ),
            ("HOME".to_string(), "/tmp/claude-home".to_string()),
        ]
    );

    // Codex takes its home as CODEX_HOME, and has no launch args.
    let launch_env = LaunchEnv {
        env: Vec::new(),
        home: settings.provider(ProviderKind::Codex).home_path.clone(),
    };
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
    let opts = session_options(&meta, &settings, launch_env, None, None, None, None);
    assert!(opts.extra_args.is_empty());
    assert_eq!(
        opts.launch_env.pairs(ProviderKind::Codex),
        vec![("CODEX_HOME".to_string(), "/tmp/codex-shadow".to_string())]
    );

    // pi project trust is opt-in and appends --approve after launch args.
    let meta = SessionMeta::new(ProviderKind::Pi, PathBuf::from("/x"), None);
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        None,
        None,
        None,
        None,
    );
    assert_eq!(opts.extra_args, vec!["--verbose"]);

    settings
        .provider_mut(ProviderKind::Pi)
        .pi
        .trust_project_extensions = true;
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        None,
        None,
        None,
        None,
    );
    assert_eq!(opts.extra_args, vec!["--verbose", "--approve"]);
}

/// Sensitive env rows contribute their value from `secrets.json`, never from
/// settings.json (which stores an empty value for them).
#[test]
fn launch_env_merges_secrets_for_sensitive_rows() {
    let test_store = TestStore::new("tcode-env-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);
    let mut settings = state.settings.clone();
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
        // A sensitive row whose secret was never saved contributes nothing.
        EnvVar {
            name: "UNSET_SECRET".into(),
            value: String::new(),
            sensitive: true,
        },
    ];
    state.settings = settings;
    state
        .settings_store
        .set_profile_secret(
            Settings::builtin_profile_id(ProviderKind::ClaudeCode),
            "ANTHROPIC_API_KEY",
            Some("sk-x"),
        )
        .unwrap();

    let profile_id = Settings::builtin_profile_id(ProviderKind::ClaudeCode);
    let env = launch_env_for_profile(
        &state.settings,
        profile_id,
        state.settings_store.profile_secrets(profile_id),
    )
    .env;
    assert_eq!(
        env,
        vec![
            ("PLAIN".to_string(), "visible".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "sk-x".to_string()),
        ]
    );
}

#[test]
fn provider_snapshots_are_isolated_by_profile() {
    let test_store = TestStore::new("tcode-profile-snapshot-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);
    state.providers.provider_snapshots.insert(
        "claude".into(),
        ProviderSnapshot {
            version: Some("1.0.0".into()),
            ..ProviderSnapshot::default()
        },
    );
    state.providers.provider_snapshots.insert(
        "kimi".into(),
        ProviderSnapshot {
            version: Some("2.0.0".into()),
            ..ProviderSnapshot::default()
        },
    );

    assert_eq!(
        state
            .profile_snapshot("kimi")
            .and_then(|snapshot| snapshot.version.as_deref()),
        Some("2.0.0")
    );
    assert_eq!(
        state
            .profile_snapshot("claude")
            .and_then(|snapshot| snapshot.version.as_deref()),
        Some("1.0.0")
    );
    assert_eq!(
        state
            .provider_snapshot(ProviderKind::ClaudeCode)
            .and_then(|snapshot| snapshot.version.as_deref()),
        Some("1.0.0")
    );
}

#[test]
fn profile_binary_override_wins_over_path_lookup() {
    let test_store = TestStore::new("tcode-profile-binary-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);
    state.settings.profiles.insert(
        "kimi".into(),
        ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                binary_path: Some(PathBuf::from("/opt/kimi/claude")),
                ..ProviderSettings::default()
            },
        },
    );

    assert_eq!(
        state.resolve_profile_binary("kimi"),
        Some(PathBuf::from("/opt/kimi/claude"))
    );
}

/// A third-party Claude profile ("Klaude Kode" → Kimi) launches against its
/// own endpoint, binary, and key, in parallel with the untouched official
/// Claude profile. This is the end-to-end proof of profile-ization at the
/// launch layer.
#[test]
fn third_party_profile_launches_in_parallel_with_builtin() {
    let test_store = TestStore::new("tcode-profile-env");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);

    let mut settings = state.settings.clone();
    // Official Claude keeps its own key.
    settings.provider_mut(ProviderKind::ClaudeCode).env = vec![EnvVar {
        name: "ANTHROPIC_API_KEY".into(),
        value: String::new(),
        sensitive: true,
    }];
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

    // The profile's launch env carries the Kimi endpoint + its own key.
    let env = launch_env_for_profile(
        &state.settings,
        "klaude-kode",
        state.settings_store.profile_secrets("klaude-kode"),
    )
    .env;
    assert!(env.contains(&(
        "ANTHROPIC_BASE_URL".to_string(),
        "https://api.kimi.com/coding/".to_string()
    )));
    assert!(env.contains(&("ANTHROPIC_MODEL".to_string(), "k3[1m]".to_string())));
    assert!(env.contains(&("ANTHROPIC_API_KEY".to_string(), "sk-kimi".to_string())));

    // The built-in profile is untouched: official key, no third-party URL.
    let builtin_profile_id = Settings::builtin_profile_id(ProviderKind::ClaudeCode);
    let builtin = launch_env_for_profile(
        &state.settings,
        builtin_profile_id,
        state.settings_store.profile_secrets(builtin_profile_id),
    )
    .env;
    assert!(builtin.contains(&("ANTHROPIC_API_KEY".to_string(), "sk-official".to_string())));
    assert!(!builtin.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));

    // A session bound to the profile resolves the profile's env + binary,
    // while its protocol stays ClaudeCode.
    let mut meta = SessionMeta::new(
        ProviderKind::ClaudeCode,
        PathBuf::from("/x"),
        Some("k3[1m]".into()),
    );
    meta.profile_id = Some("klaude-kode".into());
    let launch_env = session_launch_env(&state.settings, &state.settings_store, &meta);
    assert!(
        launch_env
            .env
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://api.kimi.com/coding/")
    );
    let opts = session_options(&meta, &state.settings, launch_env, None, None, None, None);
    assert_eq!(opts.binary_path, Some(PathBuf::from("/opt/kimi/claude")));
}

#[test]
fn session_options_injects_mcp_registration() {
    let settings = Settings::default();
    let meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
    let reg = agent::McpRegistration {
        name: agent::McpRegistration::SERVER_NAME_PREVIEW.into(),
        url: "http://127.0.0.1:7/mcp".into(),
        bearer_token: "tok".into(),
    };
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        Some(reg),
        None,
        None,
        None,
    );
    let mcp = opts
        .mcp_servers
        .first()
        .expect("registration threaded through");
    assert_eq!(mcp.url, "http://127.0.0.1:7/mcp");
    assert_eq!(mcp.bearer_token, "tok");
}

#[test]
fn pi_session_options_coerce_modes_and_drop_preview_without_native_approvals() {
    let settings = Settings::default();
    let mut meta = SessionMeta::new(ProviderKind::Pi, PathBuf::from("/x"), None);
    meta.approval_mode = ApprovalMode::Supervised;
    let reg = agent::McpRegistration {
        name: agent::McpRegistration::SERVER_NAME_PREVIEW.into(),
        url: "http://127.0.0.1:7/mcp".into(),
        bearer_token: "tok".into(),
    };

    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        Some(reg),
        None,
        None,
        None,
    );

    assert_eq!(opts.approval_mode, ApprovalMode::FullAccess);
    assert!(opts.mcp_servers.is_empty());
    assert_eq!(meta.approval_mode, ApprovalMode::Supervised);

    meta.approval_mode = ApprovalMode::AutoAcceptEdits;
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        None,
        None,
        None,
        None,
    );
    assert_eq!(opts.approval_mode, ApprovalMode::FullAccess);

    meta.approval_mode = ApprovalMode::ReadOnly;
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        None,
        None,
        None,
        None,
    );
    assert_eq!(opts.approval_mode, ApprovalMode::ReadOnly);

    meta.approval_mode = ApprovalMode::FullAccess;
    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        None,
        None,
        None,
        None,
    );
    assert_eq!(opts.approval_mode, ApprovalMode::FullAccess);
}

#[test]
fn pi_session_options_preserve_supervised_with_native_approvals() {
    let mut settings = Settings::default();
    settings.provider_mut(ProviderKind::Pi).pi.native_approvals = true;
    let mut meta = SessionMeta::new(ProviderKind::Pi, PathBuf::from("/x"), None);
    meta.approval_mode = ApprovalMode::Supervised;

    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        None,
        None,
        None,
        None,
    );

    assert_eq!(opts.approval_mode, ApprovalMode::Supervised);
}

#[test]
fn non_pi_session_options_preserve_mode_and_preview_registration() {
    let settings = Settings::default();
    let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
    meta.approval_mode = ApprovalMode::AutoAcceptEdits;
    let reg = agent::McpRegistration {
        name: agent::McpRegistration::SERVER_NAME_PREVIEW.into(),
        url: "http://127.0.0.1:7/mcp".into(),
        bearer_token: "tok".into(),
    };

    let opts = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        Some(reg),
        None,
        None,
        None,
    );

    assert_eq!(opts.approval_mode, ApprovalMode::AutoAcceptEdits);
    assert_eq!(opts.mcp_servers.len(), 1);
}

#[test]
fn session_options_isolates_orchestrate_registration_by_meta_flag() {
    let settings = Settings::default();
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
    let registration = agent::McpRegistration {
        name: agent::McpRegistration::SERVER_NAME_ORCHESTRATE.into(),
        url: "http://127.0.0.1:8/mcp".into(),
        bearer_token: "parent-token".into(),
    };
    let normal = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        None,
        Some(registration.clone()),
        None,
        None,
    );
    assert!(normal.mcp_servers.is_empty());

    meta.orchestrate_enabled = true;
    let enabled = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        None,
        Some(registration),
        None,
        None,
    );
    assert_eq!(
        enabled.mcp_servers[0].name,
        agent::McpRegistration::SERVER_NAME_ORCHESTRATE
    );
}

#[test]
fn session_options_gates_computer_use_registration_on_global_setting() {
    let mut settings = Settings::default();
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
    let registration = agent::McpRegistration {
        name: agent::McpRegistration::SERVER_NAME_COMPUTER_USE.into(),
        url: "http://127.0.0.1:9/mcp".into(),
        bearer_token: "computer-token".into(),
    };

    let disabled = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        None,
        None,
        None,
        Some(registration.clone()),
    );
    assert!(disabled.mcp_servers.is_empty());

    settings.computer_use.enabled = true;
    let enabled = session_options(
        &meta,
        &settings,
        LaunchEnv::default(),
        None,
        None,
        None,
        Some(registration),
    );
    assert_eq!(
        enabled.mcp_servers[0].name,
        agent::McpRegistration::SERVER_NAME_COMPUTER_USE
    );
}

#[test]
fn child_meta_links_parent_project_and_maps_effort() {
    let mut parent = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/p"), None);
    parent.id = "parent".into();
    parent.project_id = Some("project".into());
    let child = build_child_meta(
        &parent,
        ProviderKind::Codex,
        Some("gpt-test".into()),
        Some("high".into()),
        Some("work-codex".into()),
        ApprovalMode::AutoAcceptEdits,
        PathBuf::from("/p/sub"),
        true,
        Some(2400),
    );
    assert_eq!(child.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(child.project_id.as_deref(), Some("project"));
    assert_eq!(child.model.as_deref(), Some("gpt-test"));
    assert_eq!(child.profile_id.as_deref(), Some("work-codex"));
    assert_eq!(child.approval_mode, ApprovalMode::AutoAcceptEdits);
    assert!(child.archive_on_complete);
    assert_eq!(child.result_max_chars, Some(2400));
    assert_eq!(child.option_selections.len(), 1);
    assert_eq!(child.option_selections[0].id, "reasoningEffort");
    assert_eq!(child.option_selections[0].value, serde_json::json!("high"));
}

#[test]
fn callback_text_is_a_compact_digest_with_usage() {
    let text = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        "done",
        None,
        None,
        None,
        false,
    );
    assert!(text.starts_with("[orchestrate] thread child (\"Title\") completed.\n"));
    assert!(text.ends_with("\ndone"));
    assert!(!text.contains("tokens:"));
    assert!(
        assemble_callback_text(
            "child",
            "Title",
            TurnStatus::Completed,
            "",
            None,
            None,
            None,
            false
        )
        .ends_with("\n(no assistant output)")
    );

    let archived = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        "done",
        None,
        None,
        None,
        true,
    );
    assert!(archived.contains("completed (auto-archived; send revives it)."));

    let long = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        &"x".repeat(5000),
        None,
        None,
        None,
        false,
    );
    assert!(long.contains(
        "Final output tail (5000 chars total; the tail plus the diff is usually enough — result child has the full text):"
    ));
    assert_eq!(long.lines().last().unwrap().chars().count(), 600);

    let unlimited = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        &"x".repeat(5000),
        None,
        None,
        Some(0),
        false,
    );
    assert_eq!(unlimited.lines().last().unwrap().chars().count(), 5000);
    assert!(!unlimited.contains("Final output tail"));

    let capped = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        &"x".repeat(5000),
        None,
        None,
        Some(300),
        false,
    );
    assert!(capped.contains(
        "Final output tail (5000 chars total; the tail plus the diff is usually enough — result child has the full text):"
    ));
    assert_eq!(capped.lines().last().unwrap().chars().count(), 300);

    let usage = agent::TokenUsage {
        input_tokens: Some(100),
        cached_input_tokens: Some(25),
        output_tokens: Some(40),
        total_processed_tokens: Some(165),
        ..Default::default()
    };
    let failed = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Interrupted,
        "done",
        None,
        Some(&usage),
        None,
        false,
    );
    assert!(failed.starts_with("[orchestrate] thread child (\"Title\") failed. tokens:"));
    assert!(failed.ends_with("\ndone"));
    assert!(failed.contains("tokens: input 100 (+25 cached), output 40, total 165."));
}

#[test]
fn callback_prefers_reported_result_in_full() {
    let report = "R".repeat(5000);
    let text = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        "final message",
        Some(&report),
        None,
        Some(300),
        false,
    );
    // The reported text wins over the final message and ignores the cap.
    assert!(text.contains("Result (reported via report_result):"));
    assert!(text.ends_with(&report));
    assert!(!text.contains("final message"));
    assert!(!text.contains("Final output tail"));

    // A blank report falls back to the ordinary digest.
    let blank = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        "final message",
        Some("  \n"),
        None,
        None,
        false,
    );
    assert!(blank.ends_with("\nfinal message"));
}

#[test]
fn short_report_appends_final_message_digest() {
    let final_message = "f".repeat(3000);
    let text = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        &final_message,
        Some("done."),
        None,
        None,
        false,
    );
    assert!(text.contains("Result (reported via report_result):\ndone."));
    assert!(text.contains("The report is brief; the final assistant message follows:"));
    assert!(text.contains("Final output tail (3000 chars total"));

    // A substantive report stands alone.
    let report = "R".repeat(400);
    let alone = assemble_callback_text(
        "child",
        "Title",
        TurnStatus::Completed,
        &final_message,
        Some(&report),
        None,
        None,
        false,
    );
    assert!(alone.ends_with(&report));
    assert!(!alone.contains("The report is brief"));
}

#[test]
fn dispatched_brief_carries_report_contract_footer() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-brief-footer-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    state.host_update(cx, |state, cx| {
        let parent = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
        let parent_id = parent.id.clone();
        state.sessions.push(parent);
        let child_id = state
            .create_child_session(
                &parent_id,
                ProviderKind::Codex,
                Some("gpt-test".into()),
                None,
                None,
                ApprovalMode::FullAccess,
                "Child".into(),
                None,
                "Inspect the workspace".into(),
                true,
                None,
                cx,
            )
            .unwrap();
        let queued = state.resident(&child_id).unwrap().queue[0].text.clone();
        assert!(queued.starts_with("Inspect the workspace"));
        assert!(queued.contains("report_result"), "brief: {queued}");
    });
}

#[test]
fn terminal_callback_archives_only_when_requested() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-callback-archive-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (parent_commands, _parent_receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        for (id, archive_on_complete, status) in [
            ("auto", true, TurnStatus::Completed),
            ("keep", false, TurnStatus::Completed),
            ("retry", true, TurnStatus::Failed),
        ] {
            let (commands, _receiver) = smol::channel::unbounded();
            let mut child = live_session(ProviderKind::Codex, commands);
            child.meta.id = id.into();
            child.meta.parent_session_id = Some("parent".into());
            child.meta.archive_on_complete = archive_on_complete;
            child.turn_in_flight = true;
            state.sessions.push(child.meta.clone());
            state.residents.parked.insert(child.meta.id.clone(), child);

            state.on_event(id, persisted_assistant_event("done"), cx);
            state.on_event(
                id,
                AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{id}"),
                    status,
                    usage: None,
                },
                cx,
            );
        }
    });

    cx.run_until_parked();

    state.read_with(cx, |state, _| {
        assert!(
            state.find_meta("auto").unwrap().archived_at.is_some(),
            "archive_on_complete child should be archived after callback delivery"
        );
        assert!(
            state.find_meta("keep").unwrap().archived_at.is_none(),
            "control child should remain unarchived"
        );
        assert!(
            state.find_meta("retry").unwrap().archived_at.is_none(),
            "failed child should stay visible for retries"
        );
    });
}

#[test]
fn reported_result_reaches_parent_and_fallback_covers_silent_children() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-report-result-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        for id in ["reporter", "silent"] {
            let (commands, _receiver) = smol::channel::unbounded();
            let mut child = live_session(ProviderKind::Codex, commands);
            child.meta.id = id.into();
            child.meta.parent_session_id = Some("parent".into());
            child.meta.archive_on_complete = false;
            child.turn_in_flight = true;
            state.sessions.push(child.meta.clone());
            state.residents.parked.insert(child.meta.id.clone(), child);
        }

        // A report from a session that is not an orchestrated child is refused.
        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::ReportResult {
                child_id: "parent".into(),
                text: "nope".into(),
            },
            reply,
            cx,
        );
        assert!(response.try_recv().unwrap().is_err());

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::ReportResult {
                child_id: "reporter".into(),
                text: "the full reported RESULT".into(),
            },
            reply,
            cx,
        );
        assert!(response.try_recv().unwrap().is_ok());

        for id in ["reporter", "silent"] {
            state.on_event(id, persisted_assistant_event("last message"), cx);
            state.on_event(
                id,
                AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{id}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
                cx,
            );
        }
    });

    cx.run_until_parked();

    let mut callbacks = Vec::new();
    while let Ok(command) = parent_receiver.try_recv() {
        if let SessionCommand::Steer { text, .. } = command {
            callbacks.push(text);
        }
    }
    assert_eq!(callbacks.len(), 2, "one callback per completed child");
    let reported = callbacks
        .iter()
        .find(|text| text.contains("thread reporter"))
        .unwrap();
    assert!(reported.contains("Result (reported via report_result):\nthe full reported RESULT"));
    assert!(!reported.contains("last message"));
    let silent = callbacks
        .iter()
        .find(|text| text.contains("thread silent"))
        .unwrap();
    assert!(silent.ends_with("\nlast message"));

    state.read_with(cx, |state, _| {
        assert!(
            state.child_reported_results.is_empty(),
            "delivery should consume the stored report"
        );
    });
}

#[test]
fn orchestrate_send_unarchives_the_child() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-send-unarchive-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, _receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut child = live_session(ProviderKind::Codex, commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        child.meta.archived_at = Some(1);
        child.turn_in_flight = true;
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Send {
                parent_id: "parent".into(),
                thread_id: "child".into(),
                message: "retry with the failing test".into(),
            },
            reply,
            cx,
        );
        assert!(response.try_recv().unwrap().is_ok());
        assert!(
            state.find_meta("child").unwrap().archived_at.is_none(),
            "send should revive an archived child"
        );
    });
}

#[test]
fn orchestrate_archive_is_batch_atomic_and_parent_scoped() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-archive-op-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        for (id, parent_id) in [
            ("child-a", "parent"),
            ("child-b", "parent"),
            ("foreign", "other-parent"),
        ] {
            let mut meta =
                SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
            meta.id = id.into();
            meta.parent_session_id = Some(parent_id.into());
            state.sessions.push(meta);
        }

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Archive {
                parent_id: "parent".into(),
                thread_ids: vec!["child-a".into(), "missing".into(), "foreign".into()],
            },
            reply,
            cx,
        );
        let error = response.try_recv().unwrap().unwrap_err();
        assert!(error.contains("missing"));
        assert!(error.contains("foreign"));
        assert!(state.find_meta("child-a").unwrap().archived_at.is_none());
        assert!(state.find_meta("child-b").unwrap().archived_at.is_none());

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Archive {
                parent_id: "parent".into(),
                thread_ids: vec!["child-a".into(), "child-b".into()],
            },
            reply,
            cx,
        );
        assert_eq!(
            response.try_recv().unwrap().unwrap(),
            serde_json::json!({
                "ok": true,
                "archived": 2,
                "thread_ids": ["child-a", "child-b"],
            })
        );
        assert!(state.find_meta("child-a").unwrap().archived_at.is_some());
        assert!(state.find_meta("child-b").unwrap().archived_at.is_some());
        let archived = state.find_meta("child-a").unwrap();
        assert_eq!(
            state.child_status_json(&archived, &Timeline::default())["archived"],
            true
        );
    });
}

#[test]
fn child_approval_request_sends_exactly_one_parent_callback() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-approval-callback-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        let mut child = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
        child.id = "child".into();
        child.title = "Read-only review".into();
        child.parent_session_id = Some("parent".into());
        state.sessions.push(child.clone());

        let request = agent::ApprovalRequest {
            id: "approval-1".into(),
            turn_id: Some("turn-1".into()),
            kind: agent::ApprovalKind::ExecCommand {
                command: "touch blocked".into(),
                cwd: Some("/tmp/project".into()),
                reason: None,
            },
            options: Vec::new(),
        };
        state.on_event("child", AgentEvent::ApprovalRequested(request.clone()), cx);
        state.on_event("child", AgentEvent::ApprovalRequested(request), cx);

        let SessionCommand::Steer { text, .. } = receiver.try_recv().unwrap() else {
            panic!("approval callback did not steer the parent")
        };
        assert!(text.starts_with("[orchestrate] thread child"));
        assert!(text.contains("waiting for approval: command `touch blocked`"));
        assert!(text.contains("request_id: approval-1"));
        assert!(text.contains("decide with the approve tool"));
        assert!(receiver.try_recv().is_err(), "callback was delivered twice");

        let status = state.child_status_json(&child, &Timeline::default());
        assert_eq!(
            status["waiting_approval"],
            serde_json::json!("command `touch blocked`")
        );
        assert_eq!(
            status["approval_request_id"],
            serde_json::json!("approval-1")
        );
    });
}

#[test]
fn child_approval_always_allow_responds_without_parent_callback() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-approval-auto-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();
    let (child_commands, child_receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        state.settings.orchestrate.child_approval = ChildApprovalMode::AlwaysAllow;
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        let mut child = live_session(ProviderKind::Codex, child_commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        state.on_event(
            "child",
            AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval-auto".into(),
                turn_id: None,
                kind: agent::ApprovalKind::ExecCommand {
                    command: "touch allowed".into(),
                    cwd: None,
                    reason: None,
                },
                options: Vec::new(),
            }),
            cx,
        );

        assert!(matches!(
            child_receiver.try_recv(),
            Ok(SessionCommand::RespondApproval {
                request_id,
                decision: ApprovalDecision::ApproveForSession,
            }) if request_id == "approval-auto"
        ));
        assert!(
            parent_receiver.try_recv().is_err(),
            "always-allow must not notify the parent"
        );
    });
}

#[test]
fn child_report_result_approval_is_auto_approved_in_every_mode() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-approval-report-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();
    let (child_commands, child_receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        // Default routing (Orchestrator) — the report tool must never reach it.
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        let mut child = live_session(ProviderKind::ClaudeCode, child_commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        state.on_event(
            "child",
            AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval-report".into(),
                turn_id: None,
                kind: agent::ApprovalKind::ToolUse {
                    name: "mcp__tcode_report__report_result".into(),
                    input: serde_json::json!({ "text": "full report" }),
                    detail: "mcp__tcode_report__report_result".into(),
                },
                options: Vec::new(),
            }),
            cx,
        );

        assert!(matches!(
            child_receiver.try_recv(),
            Ok(SessionCommand::RespondApproval {
                request_id,
                decision: ApprovalDecision::ApproveForSession,
            }) if request_id == "approval-report"
        ));
        assert!(
            parent_receiver.try_recv().is_err(),
            "the report tool must not surface an approval to the orchestrator"
        );
    });
}

#[test]
fn child_approval_manual_preserves_legacy_notice_without_auto_response() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-approval-manual-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (parent_commands, parent_receiver) = smol::channel::unbounded();
    let (child_commands, child_receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        state.settings.orchestrate.child_approval = ChildApprovalMode::Manual;
        let mut parent = live_session(ProviderKind::Codex, parent_commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state.residents.parked.insert(parent.meta.id.clone(), parent);

        let mut child = live_session(ProviderKind::Codex, child_commands);
        child.meta.id = "child".into();
        child.meta.title = "Manual child".into();
        child.meta.parent_session_id = Some("parent".into());
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        state.on_event(
            "child",
            AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval-manual".into(),
                turn_id: None,
                kind: agent::ApprovalKind::ExecCommand {
                    command: "touch blocked".into(),
                    cwd: None,
                    reason: None,
                },
                options: Vec::new(),
            }),
            cx,
        );

        let SessionCommand::Steer { text, .. } = parent_receiver.try_recv().unwrap() else {
            panic!("manual approval notice did not reach the parent")
        };
        assert_eq!(
            text,
            "[orchestrate] thread child (\"Manual child\") is waiting for approval: command `touch blocked`."
        );
        assert!(child_receiver.try_recv().is_err());
    });
}

#[test]
fn orchestrate_approve_routes_decisions_and_validates_scope() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-approve-op-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut child = live_session(ProviderKind::Codex, commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some("parent".into());
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);
        state.record_approval_event(
            "child",
            &AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval-op".into(),
                turn_id: None,
                kind: agent::ApprovalKind::ExecCommand {
                    command: "cargo test".into(),
                    cwd: None,
                    reason: None,
                },
                options: Vec::new(),
            }),
        );

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Approve {
                parent_id: "parent".into(),
                thread_id: "child".into(),
                request_id: None,
                decision: " APPROVE ".into(),
            },
            reply,
            cx,
        );
        let result = response.try_recv().unwrap().unwrap();
        assert_eq!(
            result,
            serde_json::json!({ "ok": true, "request_id": "approval-op" })
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::RespondApproval {
                request_id,
                decision: ApprovalDecision::Approve,
            }) if request_id == "approval-op"
        ));

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Approve {
                parent_id: "parent".into(),
                thread_id: "child".into(),
                request_id: Some("missing".into()),
                decision: "deny".into(),
            },
            reply,
            cx,
        );
        let unknown_request = response.try_recv().unwrap().unwrap_err();
        assert_eq!(unknown_request, "no pending approval with that request_id");

        // The successful response above clears the request immediately. Seed a
        // fresh provider request before independently exercising validation.
        state.record_approval_event(
            "child",
            &AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval-op".into(),
                turn_id: None,
                kind: agent::ApprovalKind::ExecCommand {
                    command: "cargo test".into(),
                    cwd: None,
                    reason: None,
                },
                options: Vec::new(),
            }),
        );

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Approve {
                parent_id: "parent".into(),
                thread_id: "child".into(),
                request_id: Some("approval-op".into()),
                decision: "later".into(),
            },
            reply,
            cx,
        );
        let bad_decision = response.try_recv().unwrap().unwrap_err();
        assert_eq!(
            bad_decision,
            "unknown decision: later; expected approve, approve_for_session, or deny"
        );

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Approve {
                parent_id: "other-parent".into(),
                thread_id: "child".into(),
                request_id: Some("approval-op".into()),
                decision: "deny".into(),
            },
            reply,
            cx,
        );
        let non_child = response.try_recv().unwrap().unwrap_err();
        assert_eq!(non_child, "unknown thread or not a child of this parent");
    });
}

#[test]
fn final_assistant_message_joins_all_blocks_of_the_final_output() {
    let timeline = Timeline::fold_events([
        AgentEvent::ItemCompleted(ThreadItem {
            id: "preamble".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "Earlier tool preamble.".into(),
            },
        }),
        AgentEvent::ItemCompleted(ThreadItem {
            id: "reasoning".into(),
            parent_item_id: None,
            content: ItemContent::Reasoning {
                text: "private reasoning".into(),
            },
        }),
        AgentEvent::ItemCompleted(ThreadItem {
            id: "final-1".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "Complete ".into(),
            },
        }),
        AgentEvent::ItemCompleted(ThreadItem {
            id: "final-2".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "answer.".into(),
            },
        }),
    ]);

    assert_eq!(final_assistant_message(&timeline), "Complete answer.");
}

#[test]
fn resident_background_child_result_uses_completed_live_timeline() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-resident-result-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let report = format!("Complete child report:\n{}", "full detail ".repeat(80));

    state.host_update(cx, |state, cx| {
        let mut parent = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
        parent.id = "parent".into();

        let (commands, _receiver) = smol::channel::unbounded();
        let mut child = live_session(ProviderKind::Codex, commands);
        child.meta.id = "child".into();
        child.meta.parent_session_id = Some(parent.id.clone());
        child.turn_in_flight = true;

        state.sessions.push(parent);
        state.sessions.push(child.meta.clone());
        state.residents.parked.insert(child.meta.id.clone(), child);

        state.on_event("child", persisted_assistant_event(&report), cx);
        state.on_event(
            "child",
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );

        let child = state.residents.parked.get("child").unwrap();
        assert!(
            child.idle_since.is_some(),
            "completed child must remain resident in background"
        );
        assert!(!child.turn_in_flight);

        let (reply, response) = smol::channel::bounded(1);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Result {
                parent_id: "parent".into(),
                thread_id: "child".into(),
            },
            reply,
            cx,
        );
        let result = response.try_recv().unwrap().unwrap();
        assert_eq!(result["state"], "completed");
        assert_eq!(result["final_message"], report);
    });
}

#[test]
fn steering_parked_orchestrator_callback_uses_recorded_id() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-steer-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();
    let mut recorded_request_id = String::new();

    state.host_update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::Codex, commands);
        parent.meta.id = "parent".into();
        parent.turn_in_flight = true;
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        state.deliver_orchestrate_callback_to_parent(
            "parent",
            "[orchestrate] child-a completed.\nfull result".into(),
            cx,
        );

        let parent = state.residents.parked.get("parent").unwrap();
        assert!(parent.queue.is_empty(), "parallel result must not queue");
        assert!(parent.turn_in_flight);
        let command = receiver.try_recv().unwrap();
        let SessionCommand::Steer {
            request_id, text, ..
        } = command
        else {
            panic!("callback did not steer")
        };
        recorded_request_id = request_id;
        assert!(text.contains("full result"));
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let timeline = Timeline::fold_events(state.store.read_events("parent"));
        assert!(timeline.entries.iter().any(|entry| matches!(
            &entry.content,
            EntryContent::Steer {
                text,
                status: tcode_core::session::SteeringStatus::Pending,
                ..
            } if entry.id == recorded_request_id && text.contains("child-a completed")
        )));
    });
}

#[test]
fn steering_user_and_queue_paths_send_the_same_id_they_record() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-user-steer-id-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "active".into();
        active.turn_in_flight = true;
        active.timeline.apply_at(
            None,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "opening".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "start".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
        );
        state.residents.active = Some(active);

        state.steer("redirect".into(), Vec::new(), cx);
        let SessionCommand::Steer { request_id, .. } = receiver.try_recv().unwrap() else {
            panic!("user steer command missing")
        };
        let active = state.residents.active.as_ref().unwrap();
        assert!(active.timeline.entries.iter().any(|entry| matches!(
            &entry.content,
            EntryContent::Steer {
                text,
                status: tcode_core::session::SteeringStatus::Pending,
                ..
            } if entry.id == request_id && text == "redirect"
        )));

        let queued_id = state
            .residents
            .active
            .as_mut()
            .unwrap()
            .push_queued("queued redirect".into(), Vec::new());
        state.steer_queued(queued_id, cx);
        let SessionCommand::Steer { request_id, .. } = receiver.try_recv().unwrap() else {
            panic!("queue-to-steer command missing")
        };
        let active = state.residents.active.as_ref().unwrap();
        assert!(active.queue.is_empty());
        assert!(active.timeline.entries.iter().any(|entry| matches!(
            &entry.content,
            EntryContent::Steer {
                text,
                status: tcode_core::session::SteeringStatus::Pending,
                ..
            } if entry.id == request_id && text == "queued redirect"
        )));
    });
}

#[test]
fn callbacks_racing_provider_start_share_one_wakeup_turn() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-orchestrate-start-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        let mut parent = live_session(ProviderKind::ClaudeCode, smol::channel::unbounded().0);
        parent.meta.id = "parent".into();
        parent.runtime = Runtime::Starting { generation: 1 };
        state
            .residents
            .parked
            .insert(parent.meta.id.clone(), parent);

        state.deliver_orchestrate_callback_to_parent(
            "parent",
            "[orchestrate] child-a completed.\nresult a".into(),
            cx,
        );
        state.deliver_orchestrate_callback_to_parent(
            "parent",
            "[orchestrate] child-b completed.\nresult b".into(),
            cx,
        );

        let parent = state.residents.parked.get("parent").unwrap();
        assert_eq!(parent.queue.len(), 1);
        assert_eq!(parent.queue[0].kind, QueuedMessageKind::OrchestrateCallback);
        assert!(parent.queue[0].text.contains("result a"));
        assert!(parent.queue[0].text.contains("result b"));

        let (commands, receiver) = smol::channel::unbounded();
        state.residents.parked.get_mut("parent").unwrap().runtime = Runtime::Live(commands);
        state.on_background_turn_completed("parent", cx);

        let delivery_id = match receiver.try_recv() {
            Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) if text.contains("result a") && text.contains("result b") => delivery_id,
            other => panic!("expected merged callback SendTurn, got {other:?}"),
        };
        assert_eq!(state.residents.parked["parent"].queue.len(), 1);
        state.on_event("parent", AgentEvent::TurnAccepted { delivery_id }, cx);
        let parent = state.residents.parked.get("parent").unwrap();
        assert!(parent.queue.is_empty());
        assert!(parent.turn_in_flight);
    });
}

#[test]
fn shutdown_active_notifies_live_provider() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-app-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();
    let active = ActiveSession {
        meta: SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None),
        timeline: Timeline::default(),
        git_branch: None,
        branches: Vec::new(),
        draft: false,
        pending_relay: None,
        runtime: Runtime::Live(commands),
        live_model: None,
        live_approval_mode: None,
        live_option_selections: Vec::new(),
        pending_ultrathink: false,
        pending_context_len: None,
        draft_workspace: WorkspaceMode::LocalCheckout,
        preparing_worktree: false,
        queue: Vec::new(),
        next_queue_id: 0,
        delivery_in_flight: None,
        turn_in_flight: false,
        background_task_count: 0,
        idle_since: None,
        provider_commands: Vec::new(),
        provider_options: Vec::new(),
        terminal_workspace: TerminalWorkspace::default(),
        _pump: None,
    };

    state.host_update(cx, |state, cx| {
        state.residents.active = Some(active);
        state.shutdown_active(cx);
        assert!(matches!(receiver.try_recv(), Ok(SessionCommand::Shutdown)));
        assert!(state.residents.active.is_none());
    });
}

/// The quit guard gates on working sessions: a session whose turn has
/// completed but which still owns provider background tasks must count as
/// working, or quitting silently kills those tasks.
#[test]
fn background_tasks_alone_count_as_working() {
    let test_store = TestStore::new("tcode-app-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);
    let (commands, _receiver) = smol::channel::unbounded();
    state.residents.active = Some(ActiveSession {
        meta: SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None),
        timeline: Timeline::default(),
        git_branch: None,
        branches: Vec::new(),
        draft: false,
        pending_relay: None,
        runtime: Runtime::Live(commands),
        live_model: None,
        live_approval_mode: None,
        live_option_selections: Vec::new(),
        pending_ultrathink: false,
        pending_context_len: None,
        draft_workspace: WorkspaceMode::LocalCheckout,
        preparing_worktree: false,
        queue: Vec::new(),
        next_queue_id: 0,
        delivery_in_flight: None,
        turn_in_flight: false,
        background_task_count: 2,
        idle_since: None,
        provider_commands: Vec::new(),
        provider_options: Vec::new(),
        terminal_workspace: TerminalWorkspace::default(),
        _pump: None,
    });

    assert!(!state.residents.active.as_ref().unwrap().turn_in_flight);
    assert_eq!(state.working_sessions_count(), 1);

    state
        .residents
        .active
        .as_mut()
        .unwrap()
        .background_task_count = 0;
    assert_eq!(state.working_sessions_count(), 0);
}

#[test]
fn queued_sends_dispatch_one_per_completed_turn() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = ActiveSession {
        meta: SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None),
        timeline: Timeline::default(),
        git_branch: None,
        branches: Vec::new(),
        draft: false,
        pending_relay: None,
        runtime: Runtime::Live(commands),
        live_model: None,
        live_approval_mode: None,
        live_option_selections: Vec::new(),
        pending_ultrathink: false,
        pending_context_len: None,
        draft_workspace: WorkspaceMode::LocalCheckout,
        preparing_worktree: false,
        queue: Vec::new(),
        next_queue_id: 0,
        delivery_in_flight: None,
        turn_in_flight: false,
        background_task_count: 0,
        idle_since: None,
        provider_commands: Vec::new(),
        provider_options: Vec::new(),
        terminal_workspace: TerminalWorkspace::default(),
        _pump: None,
    };
    active.push_queued("first".into(), Vec::new());
    active.push_queued("second".into(), Vec::new());

    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let first_delivery = match receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) if text == "first" => delivery_id,
        other => panic!("expected first SendTurn, got {other:?}"),
    };
    assert_eq!(active.dispatch_next_pending(), Ok(false));
    assert!(receiver.try_recv().is_err());
    assert_eq!(active.queue.len(), 2, "unaccepted head stays queued");
    assert_eq!(
        active.accept_turn_delivery(first_delivery).unwrap().text,
        "first"
    );
    assert_eq!(active.queue.len(), 1);
    assert_eq!(active.queue[0].text, "second");

    active.turn_in_flight = false;
    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let second_delivery = match receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) if text == "second" => delivery_id,
        other => panic!("expected second SendTurn, got {other:?}"),
    };
    active.accept_turn_delivery(second_delivery).unwrap();
    assert!(active.queue.is_empty());
}

#[test]
fn future_scheduled_head_does_not_block_ordinary_dispatch_or_acceptance() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    let scheduled_id = active.push_scheduled(
        "later".into(),
        Vec::new(),
        SystemTime::now() + Duration::from_secs(3_600),
    );
    let ordinary_id = active.push_queued("now".into(), Vec::new());

    assert_eq!(active.dispatch_next_pending(), Ok(true));
    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionCommand::SendTurn {
            delivery_id,
            text,
            ..
        }) if delivery_id == ordinary_id && text == "now"
    ));
    let accepted = active.accept_turn_delivery(ordinary_id).unwrap();
    assert_eq!(accepted.text, "now");
    assert_eq!(active.queue.len(), 1);
    assert_eq!(active.queue[0].id, scheduled_id);
}

#[test]
fn schedule_status_and_queue_actions_preserve_or_remove_deadlines() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scheduled-status-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();
    let fire_at = now_secs() + 3_600;

    state.host_update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "scheduled-active".into();
        state.residents.active = Some(active);
        state.schedule_turn("scheduled".into(), Vec::new(), fire_at, cx);

        let status = state.session_status_snapshot("scheduled-active").unwrap();
        assert_eq!(status.queued_messages.len(), 1);
        assert_eq!(status.queued_messages[0].fire_at_unix_secs, Some(fire_at));
        let scheduled_id = status.queued_messages[0].id;

        state.steer_queued(scheduled_id, cx);
        let status = state.session_status_snapshot("scheduled-active").unwrap();
        assert_eq!(status.queued_messages.len(), 1);
        assert_eq!(status.queued_messages[0].fire_at_unix_secs, None);
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { .. })
        ));

        let active = state.residents.active.as_mut().unwrap();
        active.delivery_in_flight = None;
        active.turn_in_flight = false;
        active.queue.clear();
        let drop_id = active.push_scheduled(
            "drop me".into(),
            Vec::new(),
            SystemTime::now() + Duration::from_secs(7_200),
        );
        state.drop_queued(drop_id, cx);
        assert!(state.residents.active.as_ref().unwrap().queue.is_empty());
    });
}

#[test]
fn due_scheduled_message_reenters_the_ordinary_send_path() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-scheduled-fire-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "due-active".into();
        active.push_scheduled(
            "due now".into(),
            Vec::new(),
            SystemTime::now() - Duration::from_secs(1),
        );
        state.residents.active = Some(active);

        state.fire_due_scheduled(cx);

        let active = state.residents.active.as_ref().unwrap();
        assert_eq!(active.queue.len(), 1);
        assert_eq!(active.queue[0].text, "due now");
        assert_eq!(active.queue[0].not_before, None);
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::SendTurn { text, .. }) if text == "due now"
        ));
    });
}

#[test]
fn implement_plan_waits_for_pending_relay_without_mutating_state() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-plan-relay-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::Codex, commands);
        active.meta.id = "plan-relay".into();
        active.meta.interaction_mode = InteractionMode::Plan;
        active.pending_relay = Some(PendingRelay {
            from_provider: ProviderKind::ClaudeCode,
            from_model: Some("opus".into()),
            from_profile: None,
        });
        active.timeline.apply_at(
            None,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "user-1".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "make a plan".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
        );
        active.timeline.apply_at(
            None,
            &AgentEvent::ProposedPlan {
                item_id: "plan-1".into(),
                markdown: "# Plan".into(),
            },
        );
        active.timeline.apply_at(
            None,
            &AgentEvent::TurnCompleted {
                turn_id: "plan-turn".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        );
        state.residents.active = Some(active);

        state.implement_plan(cx);

        let active = state.residents.active.as_ref().unwrap();
        assert_eq!(active.meta.interaction_mode, InteractionMode::Plan);
        assert!(active.timeline.plan_ready().is_some());
        assert!(receiver.try_recv().is_err());
    });
}

#[test]
fn profile_switch_within_one_provider_requires_a_relay() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-profile-relay-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, _receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::ClaudeCode, commands);
        active.meta.id = "profile-relay".into();
        active.meta.model = Some("claude-opus-5".into());
        active.timeline.apply_at(
            None,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "user-1".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "hello".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
        );
        active.timeline.apply_at(
            None,
            &AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        );
        state.residents.active = Some(active);

        // Same ProviderKind, different profile: a different backend. The
        // selection must park behind the relay confirmation and rebind the
        // session's profile so the next launch uses the new endpoint.
        state.set_active_model(
            ProviderKind::ClaudeCode,
            Some("kimi-k3".into()),
            Some("kimi".into()),
            cx,
        );
        let active = state.residents.active.as_ref().unwrap();
        assert_eq!(active.meta.profile_id.as_deref(), Some("kimi"));
        assert_eq!(active.meta.model.as_deref(), Some("kimi-k3"));
        let pending = active.pending_relay.as_ref().expect("relay pending");
        assert_eq!(pending.from_provider, ProviderKind::ClaudeCode);
        assert_eq!(pending.from_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(pending.from_profile, None);
        assert_eq!(
            state.relay_confirmation(),
            Some(("Claude Code".into(), "kimi".into()))
        );

        // Returning to the original profile cancels the pending relay.
        state.set_active_model(
            ProviderKind::ClaudeCode,
            Some("claude-opus-5".into()),
            None,
            cx,
        );
        let active = state.residents.active.as_ref().unwrap();
        assert!(active.pending_relay.is_none());
        assert_eq!(active.meta.profile_id, None);
        assert!(state.relay_confirmation().is_none());
    });
}

#[test]
fn queued_turns_keep_the_interaction_mode_selected_at_submit_time() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    active.turn_in_flight = true;
    active.meta.interaction_mode = InteractionMode::Plan;
    active.push_queued("plan turn".into(), Vec::new());
    active.meta.interaction_mode = InteractionMode::Build;
    active.turn_in_flight = false;

    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let first_delivery = match receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id,
            options: Some(options),
            ..
        }) => {
            assert_eq!(options.interaction_mode, Some(InteractionMode::Plan));
            delivery_id
        }
        other => panic!("expected queued Plan turn, got {other:?}"),
    };
    active.accept_turn_delivery(first_delivery).unwrap();
    active.turn_in_flight = false;

    active.meta.interaction_mode = InteractionMode::Build;
    active.push_queued("build turn".into(), Vec::new());
    active.meta.interaction_mode = InteractionMode::Plan;
    assert_eq!(active.dispatch_next_pending(), Ok(true));
    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionCommand::SendTurn {
            options: Some(TurnOptions {
                interaction_mode: Some(InteractionMode::Build),
                ..
            }),
            ..
        })
    ));
}

/// A live session with `provider`, nothing queued, no turn in flight.
fn live_session(
    provider: ProviderKind,
    commands: smol::channel::Sender<SessionCommand>,
) -> ActiveSession {
    ActiveSession {
        meta: SessionMeta::new(provider, PathBuf::from("/tmp/project"), None),
        timeline: Timeline::default(),
        provider_options: Vec::new(),
        git_branch: None,
        branches: Vec::new(),
        draft: false,
        pending_relay: None,
        runtime: Runtime::Live(commands),
        live_model: None,
        live_approval_mode: Some(ApprovalMode::default()),
        live_option_selections: Vec::new(),
        pending_ultrathink: false,
        pending_context_len: None,
        draft_workspace: WorkspaceMode::LocalCheckout,
        preparing_worktree: false,
        queue: Vec::new(),
        next_queue_id: 0,
        delivery_in_flight: None,
        turn_in_flight: false,
        background_task_count: 0,
        idle_since: None,
        provider_commands: Vec::new(),
        terminal_workspace: TerminalWorkspace::default(),
        _pump: None,
    }
}

#[test]
fn opencode_effort_is_applied_per_turn_without_restart() {
    let mut active = live_session(ProviderKind::OpenCode, smol::channel::unbounded().0);
    active.meta.option_selections.push(OptionSelection {
        id: "reasoningEffort".into(),
        value: serde_json::json!("high"),
    });

    assert_eq!(active.turn_options().effort.as_deref(), Some("high"));
    assert!(!active.options_changed_while_live());
}

#[test]
fn native_rewind_waits_for_provider_confirmation_before_pruning() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-native-rewind-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::ClaudeCode, commands);
        active.meta.id = "claude-session".into();
        for index in 1..=2 {
            active.timeline.apply_at(
                Some(index * 10),
                &AgentEvent::TurnStarted {
                    turn_id: format!("turn-{index}"),
                },
            );
            active.timeline.apply_at(
                Some(index * 10 + 1),
                &AgentEvent::TurnCheckpoint {
                    turn_id: format!("turn-{index}"),
                    checkpoint_id: format!("checkpoint-{index}"),
                },
            );
            active.timeline.apply_at(
                Some(index * 10 + 2),
                &AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{index}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            );
        }
        state.residents.active = Some(active);
        state.rewind_turn(1, RewindMode::Conversation, cx);
        assert_eq!(
            state
                .residents
                .active
                .as_ref()
                .unwrap()
                .timeline
                .turns
                .len(),
            2
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionCommand::Rewind {
                checkpoint_id,
                mode: RewindMode::Conversation,
            }) if checkpoint_id == "checkpoint-2"
        ));

        state.on_event(
            "claude-session",
            AgentEvent::RewindCompleted {
                checkpoint_id: "checkpoint-2".into(),
                mode: RewindMode::Conversation,
                prefill: Some("original prompt".into()),
            },
            cx,
        );
        assert_eq!(
            state
                .residents
                .active
                .as_ref()
                .unwrap()
                .timeline
                .turns
                .len(),
            1
        );
        assert!(!state.native_rewind_pending());
    });
    let mut serialized_prefill = None;
    while let Ok(line) = cx.outgoing_rx.try_recv() {
        let output = tcode_protocol::decode_host_line(&line).expect("decode host test output");
        if let HostMessage::Event(EventEnvelope {
            event: ServerEvent::NativeRewindPrefill { session_id, text },
            ..
        }) = output
            && session_id == "claude-session"
        {
            serialized_prefill = Some(text);
        }
    }
    assert_eq!(serialized_prefill.as_deref(), Some("original prompt"));
}

#[test]
fn turn_blocked_clears_active_session_queue_when_abort_on_model_fallback_is_enabled() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-turn-blocked-queue-test");
    let state = cx.new_entity(|_| AppState::new((*test_store).clone()));

    state.host_update(cx, |state, cx| {
        state.settings.abort_on_model_fallback = true;
        let mut active = ActiveSession::new(
            SessionMeta::new(
                ProviderKind::ClaudeCode,
                PathBuf::from("/tmp/turn-blocked"),
                Some("claude-opus-test".into()),
            ),
            false,
            Vec::new(),
        );
        let session_id = active.meta.id.clone();
        active.push_queued("do not auto-send".into(), Vec::new());
        assert!(!active.queue.is_empty());
        state.residents.active = Some(active);

        state.on_event(
            &session_id,
            AgentEvent::TurnBlocked {
                category: Some(agent::ClassifierCategory::Cyber),
                model: Some("claude-opus-test".into()),
                detail: "request blocked by classifier".into(),
            },
            cx,
        );

        assert!(state.residents.active.as_ref().unwrap().queue.is_empty());
    });

    assert!(cx.drain_outgoing().iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            topic: Topic::SessionStatus { .. },
            event: ServerEvent::ModelFallbackBlocked {
                category: Some(agent::ClassifierCategory::Cyber),
                model: Some(model),
                fallback_model: None,
                detail,
                ..
            },
        }) if model == "claude-opus-test" && detail == "request blocked by classifier"
    )));
}

#[test]
fn model_fallback_stops_active_session_when_abort_on_model_fallback_is_enabled() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-model-fallback-stop-test");
    let state = cx.new_entity(|_| AppState::new((*test_store).clone()));
    let (commands, receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        state.settings.abort_on_model_fallback = true;
        let mut active = live_session(ProviderKind::ClaudeCode, commands);
        active.meta.model = Some("claude-fable-5".into());
        active.turn_in_flight = true;
        active.timeline.apply_at(
            None,
            &AgentEvent::TurnStarted {
                turn_id: "turn-fallback".into(),
            },
        );
        let session_id = active.meta.id.clone();
        active.push_queued("do not auto-send".into(), Vec::new());
        state.residents.active = Some(active);

        state.on_event(
            &session_id,
            AgentEvent::ModelFallbackDetected {
                expected: "claude-fable-5".into(),
                actual: "claude-opus-4-8".into(),
                category: None,
                checkpoint_id: None,
                parent_tool_use_id: None,
            },
            cx,
        );

        let active = state.residents.active.as_ref().unwrap();
        assert!(active.queue.is_empty());
        assert!(matches!(active.runtime, Runtime::Idle));
        assert!(!active.turn_in_flight);
        assert!(!active.timeline.turn_running);
    });

    assert!(matches!(receiver.try_recv(), Ok(SessionCommand::Shutdown)));
    assert!(cx.drain_outgoing().iter().any(|message| matches!(
        message,
        HostMessage::Event(EventEnvelope {
            topic: Topic::SessionStatus { .. },
            event: ServerEvent::ModelFallbackBlocked {
                model: Some(model),
                fallback_model: Some(fallback_model),
                ..
            },
        }) if model == "claude-fable-5" && fallback_model == "claude-opus-4-8"
    )));
}

#[test]
fn shutdown_all_notifies_active_and_parked_live_providers() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-app-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    let (active_commands, active_receiver) = smol::channel::unbounded();
    let (parked_commands, parked_receiver) = smol::channel::unbounded();
    let parked = live_session(ProviderKind::ClaudeCode, parked_commands);
    let (other_commands, other_receiver) = smol::channel::unbounded();
    let other = live_session(ProviderKind::Acp, other_commands);
    state.host_update(cx, |state, cx| {
        state.residents.active = Some(live_session(ProviderKind::Codex, active_commands));
        state
            .residents
            .parked
            .insert(parked.meta.id.clone(), parked);
        state.residents.parked.insert(other.meta.id.clone(), other);
        state.shutdown_all(cx);

        assert!(matches!(
            active_receiver.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(matches!(
            parked_receiver.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(matches!(
            other_receiver.try_recv(),
            Ok(SessionCommand::Shutdown)
        ));
        assert!(state.residents.active.is_none());
        assert!(state.residents.parked.is_empty());
    });
}

/// Enter always queues while a turn runs; ⌘Enter steers only where the
/// provider actually supports it, and otherwise degrades to queueing.
#[test]
fn send_routing_matrix() {
    let (commands, _rx) = smol::channel::unbounded();
    let mut codex = live_session(ProviderKind::Codex, commands.clone());

    // Idle: both gestures are a plain send — there is nothing to steer into.
    assert_eq!(codex.route(false), SendRouting::Send);
    assert_eq!(codex.route(true), SendRouting::Send);

    // Turn running: Enter queues, ⌘Enter steers (Codex has `turn/steer`).
    codex.turn_in_flight = true;
    assert_eq!(codex.route(false), SendRouting::Queue);
    assert_eq!(codex.route(true), SendRouting::Steer);

    let mut claude = live_session(ProviderKind::ClaudeCode, commands.clone());
    claude.turn_in_flight = true;
    assert_eq!(claude.route(true), SendRouting::Steer);

    let mut pi = live_session(ProviderKind::Pi, commands.clone());
    pi.turn_in_flight = true;
    assert_eq!(pi.route(true), SendRouting::Steer);

    // OpenCode and ACP have no steering method, so a steer must fall back
    // to the queue rather than silently vanish.
    let mut opencode = live_session(ProviderKind::OpenCode, commands.clone());
    opencode.turn_in_flight = true;
    assert_eq!(opencode.route(true), SendRouting::QueueUnsupported);

    let mut acp = live_session(ProviderKind::Acp, commands);
    acp.turn_in_flight = true;
    assert_eq!(acp.route(false), SendRouting::Queue);
    assert_eq!(acp.route(true), SendRouting::QueueUnsupported);

    // A provider that can steer still can't while it isn't live.
    let mut dead = live_session(ProviderKind::Codex, smol::channel::unbounded().0);
    dead.runtime = Runtime::Idle;
    dead.turn_in_flight = true;
    assert_eq!(dead.route(true), SendRouting::QueueUnsupported);
}

/// Steering must not disturb the turn bookkeeping: it joins the turn already
/// in flight, so no queue entry is consumed and no new turn is opened.
/// (See examples/steer_probe.rs for the live protocol probe.)
#[test]
fn steering_does_not_disturb_turn_accounting() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    active.turn_in_flight = true;
    active.push_queued("queued".into(), Vec::new());

    assert_eq!(
        active.steer_now("steer-1".into(), "steer me".into(), Vec::new()),
        Ok(())
    );

    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionCommand::Steer { request_id, text, .. })
            if request_id == "steer-1" && text == "steer me"
    ));
    // Still exactly one turn in flight, and the queue is untouched.
    assert!(active.turn_in_flight);
    assert_eq!(active.queue.len(), 1);
    assert_eq!(active.queue[0].text, "queued");
}

/// The queue strip's steer button pulls that specific entry out (by id),
/// leaving the rest of the FIFO in order.
#[test]
fn queued_message_converts_to_steer() {
    let (commands, _rx) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    active.turn_in_flight = true;
    let first = active.push_queued("first".into(), Vec::new());
    let second = active.push_queued("second".into(), Vec::new());
    let third = active.push_queued("third".into(), Vec::new());
    assert_ne!(first, second);

    // Steer the middle one: it leaves the queue, order is preserved.
    let taken = active.take_queued(second).expect("queued message");
    assert_eq!(taken.text, "second");
    let remaining: Vec<_> = active.queue.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(remaining, ["first", "third"]);

    // Dropping the head (the ✕) leaves the tail alone.
    active.take_queued(first).expect("queued message");
    assert_eq!(active.queue.len(), 1);
    assert_eq!(active.queue[0].id, third);

    // An unknown id is a no-op, not a panic.
    assert!(active.take_queued(9999).is_none());
}

/// Ultrathink is per-send: it rides with the message it was armed for, not
/// with whatever happens to be dispatched later.
#[test]
fn ultrathink_rides_with_the_queued_message() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    active.turn_in_flight = true;
    active.pending_ultrathink = true;
    active.push_queued("deep".into(), Vec::new());
    // The flag is consumed by the message that was armed for it.
    assert!(!active.pending_ultrathink);
    active.push_queued("shallow".into(), Vec::new());

    active.turn_in_flight = false;
    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let first_delivery = match receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id, text, ..
        }) if text == "Ultrathink:\ndeep" => delivery_id,
        other => panic!("expected Ultrathink SendTurn, got {other:?}"),
    };
    active.accept_turn_delivery(first_delivery).unwrap();
    active.turn_in_flight = false;
    assert_eq!(active.dispatch_next_pending(), Ok(true));
    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionCommand::SendTurn { text, .. }) if text == "shallow"
    ));
}

/// An image-only send keeps its empty text in the transcript (the bubble
/// renders just the thumbnails) while the wire carries T3's placeholder.
#[test]
fn image_only_message_gets_placeholder_on_the_wire_only() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    let attachment = Attachment {
        media_type: "image/png".into(),
        data_base64: "AAAA".into(),
        source_path: Some("/tmp/a.png".into()),
    };
    active.push_queued(String::new(), vec![attachment.clone()]);

    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let delivery_id = match receiver.try_recv() {
        Ok(SessionCommand::SendTurn {
            delivery_id,
            text,
            attachments,
            ..
        }) => {
            assert_eq!(text, tcode_core::attachments::image_only_message());
            assert_eq!(attachments, vec![attachment]);
            delivery_id
        }
        other => panic!("expected SendTurn, got {other:?}"),
    };
    // The accepted (recorded) message keeps the user's empty text and the
    // local path for the timeline.
    let recorded = active.accept_turn_delivery(delivery_id).unwrap();
    assert_eq!(recorded.text, "");
    assert_eq!(attachment_paths(&recorded.attachments), vec!["/tmp/a.png"]);
}

#[test]
fn relay_context_rides_only_with_the_first_handoff_message() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut active = live_session(ProviderKind::Codex, commands);
    active.push_queued("continue here".into(), Vec::new());
    active.queue[0].relay_transcript = Some("# prior work".into());
    active.push_queued("follow up".into(), Vec::new());

    assert_eq!(active.dispatch_next_pending(), Ok(true));
    let first = receiver.try_recv().unwrap();
    let SessionCommand::SendTurn {
        delivery_id, text, ..
    } = first
    else {
        panic!("expected first send turn");
    };
    assert!(text.starts_with(tcode_core::relay::RELAY_PREAMBLE));
    assert!(text.contains("<conversation-transcript>\n# prior work"));
    assert!(text.contains("<new-user-message>\ncontinue here"));

    active.accept_turn_delivery(delivery_id).unwrap();
    active.turn_in_flight = false;
    assert_eq!(active.dispatch_next_pending(), Ok(true));
    assert!(matches!(
        receiver.try_recv(),
        Ok(SessionCommand::SendTurn { text, .. }) if text == "follow up"
    ));
}

#[test]
fn startup_generation_rejects_stale_same_session_attempt() {
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/project"), None);
    let mut active = ActiveSession {
        meta,
        timeline: Timeline::default(),
        git_branch: None,
        branches: Vec::new(),
        draft: false,
        pending_relay: None,
        runtime: Runtime::Starting { generation: 2 },
        live_model: None,
        live_approval_mode: None,
        live_option_selections: Vec::new(),
        pending_ultrathink: false,
        pending_context_len: None,
        draft_workspace: WorkspaceMode::LocalCheckout,
        preparing_worktree: false,
        queue: Vec::new(),
        next_queue_id: 0,
        delivery_in_flight: None,
        turn_in_flight: false,
        background_task_count: 0,
        idle_since: None,
        provider_commands: Vec::new(),
        provider_options: Vec::new(),
        terminal_workspace: TerminalWorkspace::default(),
        _pump: None,
    };

    assert!(!active.is_starting_generation(1));
    assert!(active.is_starting_generation(2));
    active.runtime = Runtime::Live(smol::channel::unbounded().0);
    assert!(!active.is_starting_generation(2));
}

#[test]
fn unaccepted_send_survives_eof_and_is_delivered_once_after_resume() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!(
        "tcode-acked-delivery-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-acked-delivery-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (session, first_actor) = fake_live_session(cwd.clone());
    let session_id = session.meta.id.clone();

    state.host_update(cx, |state, cx| {
        state.residents.active = Some(session);
        // The preceding model turn has completed, but Claude still owns a
        // background process. This is the idle-send window from the repro.
        state.on_event(
            &session_id,
            AgentEvent::TurnStarted {
                turn_id: "background-launch".into(),
            },
            cx,
        );
        state.on_event(
            &session_id,
            AgentEvent::BackgroundTasksChanged { count: 1 },
            cx,
        );
        state.on_event(
            &session_id,
            AgentEvent::TurnCompleted {
                turn_id: "background-launch".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );

        state.send_turn("survive the eof race".into(), Vec::new(), cx);
        let (delivery_id, submitted_text) = match first_actor.try_recv() {
            Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) => (delivery_id, text),
            other => panic!("expected submitted SendTurn, got {other:?}"),
        };
        let active = state.residents.active.as_ref().unwrap();
        assert_eq!(active.queue.len(), 1);
        assert_eq!(active.delivery_in_flight, Some(delivery_id));
        assert!(!state.store.read_events(&session_id).iter().any(|stored| {
            matches!(
                &stored.event,
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::UserMessage { text, .. },
                    ..
                }) if text == "survive the eof race"
            )
        }));

        // EOF wins before the first actor writes, so no TurnAccepted exists.
        state.on_event(
            &session_id,
            AgentEvent::SessionClosed {
                reason: Some("claude closed stdout".into()),
            },
            cx,
        );
        let active = state.residents.active.as_ref().unwrap();
        assert!(matches!(active.runtime, Runtime::Idle));
        assert_eq!(active.queue.len(), 1);
        assert_eq!(active.delivery_in_flight, None);

        let (resumed_commands, resumed_actor) = smol::channel::unbounded();
        state.residents.active.as_mut().unwrap().runtime = Runtime::Live(resumed_commands);
        assert_eq!(state.dispatch_next_queued(cx), Ok(true));
        let retried_delivery = match resumed_actor.try_recv() {
            Ok(SessionCommand::SendTurn {
                delivery_id: retried_id,
                text,
                ..
            }) if text == submitted_text => retried_id,
            other => panic!("expected retried SendTurn, got {other:?}"),
        };
        assert_eq!(retried_delivery, delivery_id);

        state.on_event(
            &session_id,
            AgentEvent::TurnAccepted {
                delivery_id: retried_delivery,
            },
            cx,
        );
        // A duplicate acceptance cannot remove or persist anything twice.
        state.on_event(
            &session_id,
            AgentEvent::TurnAccepted {
                delivery_id: retried_delivery,
            },
            cx,
        );
        assert!(state.residents.active.as_ref().unwrap().queue.is_empty());
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let delivered = state
            .store
            .read_events(&session_id)
            .iter()
            .filter(|stored| {
                matches!(
                    &stored.event,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::UserMessage { text, .. },
                        ..
                    }) if text == "survive the eof race"
                )
            })
            .count();
        assert_eq!(delivered, 1);
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn inferred_startup_model_updates_live_model_without_restart() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-live-model-sync-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "model-sync".into();

    state.host_update(cx, |state, cx| {
        state.residents.active = Some(session);
        state.on_event(
            "model-sync",
            AgentEvent::SessionStarted {
                provider_session_id: "provider-session".into(),
                resume: agent::ResumeCursor(serde_json::json!({
                    "session_id": "provider-session"
                })),
                model: Some("claude-sonnet-4-6".into()),
            },
            cx,
        );
        let active = state.residents.active.as_ref().unwrap();
        assert_eq!(active.meta.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(active.live_model, active.meta.model);
        assert!(!active.model_changed_while_live());

        state.send_turn("first message".into(), Vec::new(), cx);
        assert!(matches!(
            actor.try_recv(),
            Ok(SessionCommand::SendTurn { .. })
        ));
        assert!(actor.try_recv().is_err(), "phantom restart sent Shutdown");
    });
}

#[test]
fn park_active_retains_provider_with_background_tasks() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-background-park-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "background-owner".into();
    session.background_task_count = 1;
    state.host_update(cx, |state, cx| {
        state.residents.active = Some(session);
        state.park_active(cx);

        assert!(state.residents.active.is_none());
        assert_eq!(
            state.residents.parked["background-owner"].background_task_count,
            1
        );
        assert!(actor.try_recv().is_err(), "parking killed background work");
    });
}

#[test]
fn park_active_retains_idle_live_provider() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-park-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "idle-resident".into();

    state.host_update(cx, |state, cx| {
        state.residents.active = Some(session);
        state.park_active(cx);

        assert!(state.residents.active.is_none());
        assert!(matches!(
            state.residents.parked["idle-resident"].runtime,
            Runtime::Live(_)
        ));
        assert!(state.residents.parked["idle-resident"].idle_since.is_some());
        assert!(actor.try_recv().is_err(), "parking sent Shutdown");
    });
}

#[test]
fn select_session_readopts_idle_resident_without_shutdown() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-readopt-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "idle-resident".into();
    let meta = session.meta.clone();

    state.host_update(cx, |state, cx| {
        state.sessions.push(meta);
        state.residents.active = Some(session);
        state.park_active(cx);
        state.select_session("idle-resident", cx);

        let active = state.residents.active.as_ref().unwrap();
        assert_eq!(active.meta.id, "idle-resident");
        assert!(matches!(active.runtime, Runtime::Live(_)));
        assert!(active.idle_since.is_none());
        assert!(!state.residents.parked.contains_key("idle-resident"));
        assert!(actor.try_recv().is_err(), "re-adoption sent Shutdown");
    });
}

#[test]
fn resident_idle_reaper_shuts_down_untouched_provider() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-reaper-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "idle-resident".into();

    state.host_update(cx, |state, cx| {
        state.resident_idle_grace = Duration::from_millis(1);
        state.residents.active = Some(session);
        state.park_active(cx);
    });
    cx.run_until_parked();

    state.update(cx, |state, _| {
        assert!(!state.residents.parked.contains_key("idle-resident"));
        assert!(matches!(actor.try_recv(), Ok(SessionCommand::Shutdown)));
    });
}

#[test]
fn resident_idle_reaper_ignores_readopted_session() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-stale-timer-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "idle-resident".into();
    let meta = session.meta.clone();

    state.host_update(cx, |state, cx| {
        state.resident_idle_grace = Duration::from_millis(1);
        state.sessions.push(meta);
        state.residents.active = Some(session);
        state.park_active(cx);
        state.select_session("idle-resident", cx);
    });
    cx.run_until_parked();

    state.update(cx, |state, _| {
        let active = state.residents.active.as_ref().unwrap();
        assert_eq!(active.meta.id, "idle-resident");
        assert!(matches!(active.runtime, Runtime::Live(_)));
        assert!(actor.try_recv().is_err(), "stale timer sent Shutdown");
    });
}

#[test]
fn resident_idle_lru_evicts_only_oldest_provider() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-idle-resident-lru-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let base = Instant::now();
    let mut actors = Vec::new();

    state.host_update(cx, |state, cx| {
        for index in 0..MAX_IDLE_RESIDENTS {
            let (commands, actor) = smol::channel::unbounded();
            let mut resident = live_session(ProviderKind::ClaudeCode, commands);
            resident.meta.id = format!("resident-{index}");
            resident.idle_since =
                Some(base - Duration::from_secs((MAX_IDLE_RESIDENTS - index) as u64));
            state
                .residents
                .parked
                .insert(resident.meta.id.clone(), resident);
            actors.push(actor);
        }

        let (commands, newest_actor) = smol::channel::unbounded();
        let mut newest = live_session(ProviderKind::ClaudeCode, commands);
        newest.meta.id = "resident-newest".into();
        state.residents.active = Some(newest);
        state.park_active(cx);
        actors.push(newest_actor);

        assert!(!state.residents.parked.contains_key("resident-0"));
        assert_eq!(state.residents.parked.len(), MAX_IDLE_RESIDENTS);
    });

    assert!(matches!(actors[0].try_recv(), Ok(SessionCommand::Shutdown)));
    for actor in &actors[1..] {
        assert!(actor.try_recv().is_err(), "non-LRU resident was shut down");
    }
}

#[test]
fn settings_restart_waits_for_background_follow_up() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-background-restart-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, actor) = smol::channel::unbounded();
    let mut session = live_session(ProviderKind::ClaudeCode, commands);
    session.meta.id = "background-restart".into();
    session.live_model = Some("claude-opus-4-8".into());
    session.meta.model = Some("claude-sonnet-4-6".into());
    session.background_task_count = 1;

    state.host_update(cx, |state, cx| {
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .binary_path = Some("/nonexistent/tcode-test-claude".into());
        state.residents.active = Some(session);
        state.send_turn("use the new model later".into(), Vec::new(), cx);
        assert!(actor.try_recv().is_err());
        assert_eq!(state.residents.active.as_ref().unwrap().queue.len(), 1);

        // Claude publishes zero immediately before its self-invoked result;
        // the restart is still deferred until that follow-up turn closes.
        state.on_event(
            "background-restart",
            AgentEvent::BackgroundTasksChanged { count: 0 },
            cx,
        );
        assert!(actor.try_recv().is_err());
        state.on_event(
            "background-restart",
            AgentEvent::TurnStarted {
                turn_id: "task-follow-up".into(),
            },
            cx,
        );
        state.on_event(
            "background-restart",
            AgentEvent::TurnCompleted {
                turn_id: "task-follow-up".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(matches!(actor.try_recv(), Ok(SessionCommand::Shutdown)));
        assert_eq!(state.residents.active.as_ref().unwrap().queue.len(), 1);
    });
}

#[test]
fn model_switch_restarts_live_provider() {
    let (commands, receiver) = smol::channel::unbounded();
    let mut meta = SessionMeta::new(
        ProviderKind::ClaudeCode,
        PathBuf::from("/tmp/project"),
        None,
    );
    meta.model = Some("sonnet".into());
    let mut active = ActiveSession {
        meta,
        timeline: Timeline::default(),
        git_branch: None,
        branches: Vec::new(),
        draft: false,
        pending_relay: None,
        runtime: Runtime::Live(commands),
        // Process was started on "opus"; the user has since picked "sonnet".
        live_model: Some("opus".into()),
        live_approval_mode: None,
        live_option_selections: Vec::new(),
        pending_ultrathink: false,
        pending_context_len: None,
        draft_workspace: WorkspaceMode::LocalCheckout,
        preparing_worktree: false,
        queue: vec!["do it".into()],
        next_queue_id: 1,
        delivery_in_flight: None,
        turn_in_flight: false,
        background_task_count: 0,
        idle_since: None,
        provider_commands: Vec::new(),
        provider_options: Vec::new(),
        terminal_workspace: TerminalWorkspace::default(),
        _pump: None,
    };

    assert!(active.model_changed_while_live());
    active.shutdown_to_idle();

    // Live provider is told to shut down and the runtime is back to Idle,
    // while the queued turn is preserved for the restarted process.
    assert!(matches!(receiver.try_recv(), Ok(SessionCommand::Shutdown)));
    assert!(matches!(active.runtime, Runtime::Idle));
    assert_eq!(active.queue, [QueuedMessage::from("do it")]);
    assert!(!active.model_changed_while_live());

    // No restart when the selected model matches the live one.
    active.runtime = Runtime::Live(smol::channel::unbounded().0);
    active.live_model = active.meta.model.clone();
    assert!(!active.model_changed_while_live());
}

#[test]
fn archived_hidden_from_sidebar_and_unread_logic() {
    let test_store = TestStore::new("tcode-archive-test");
    let store = (*test_store).clone();
    let mut state = AppState::new(store);
    let project = Project {
        id: "p1".into(),
        name: "Proj".into(),
        root: PathBuf::from("/p"),
        created_at: 1,
    };
    state.projects = vec![project.clone()];
    let mut visible = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/p"), None);
    visible.project_id = Some(project.id.clone());
    visible.updated_at = 100;
    let mut archived = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/p"), None);
    archived.project_id = Some(project.id.clone());
    archived.updated_at = 100;
    archived.archived_at = Some(50);
    state.sessions = vec![visible.clone(), archived.clone()];

    // Sidebar groups exclude archived; the Archived view includes only it.
    let groups = group_sessions(
        &state.projects,
        &state
            .sessions
            .iter()
            .filter(|meta| meta.archived_at.is_none())
            .cloned()
            .collect::<Vec<_>>(),
        state.settings.project_sort,
    );
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].sessions.len(), 1);
    assert_eq!(groups[0].sessions[0].id, visible.id);
    let arch = group_sessions(
        &state.projects,
        &state
            .sessions
            .iter()
            .filter(|meta| meta.archived_at.is_some())
            .cloned()
            .collect::<Vec<_>>(),
        state.settings.project_sort,
    );
    assert_eq!(arch.len(), 1);
    assert_eq!(arch[0].sessions.len(), 1);
    assert_eq!(arch[0].sessions[0].id, archived.id);

    // Unread: never-visited is not unread; visited-before-update is unread;
    // visited-at-or-after-update clears it.
    assert!(!state.session_unread(&visible.id));
    state.settings.last_visited.insert(visible.id.clone(), 50);
    assert!(state.session_unread(&visible.id));
    assert!(state.sessions.iter().any(|meta| {
        meta.archived_at.is_none()
            && meta.project_id.as_deref() == Some(&project.id)
            && state.session_unread(&meta.id)
    }));
    state.settings.last_visited.insert(visible.id.clone(), 100);
    assert!(!state.session_unread(&visible.id));
    assert!(!state.sessions.iter().any(|meta| {
        meta.archived_at.is_none()
            && meta.project_id.as_deref() == Some(&project.id)
            && state.session_unread(&meta.id)
    }));
}

#[test]
fn fork_thread_clones_timeline_and_provider_cursor() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-fork-test");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    let mut source = SessionMeta::new(
        ProviderKind::Codex,
        PathBuf::from("/tmp/source-worktree"),
        Some("gpt-5.4".into()),
    );
    source.title = "Investigate parser".into();
    source.resume_cursor = Some(agent::ResumeCursor(
        serde_json::json!({"thread_id": "native-source"}),
    ));
    source.worktree = Some(WorktreeInfo {
        root_project_path: PathBuf::from("/tmp/project"),
        base: "main".into(),
        branch: "tcode/source".into(),
    });
    store.upsert_meta(&source).unwrap();
    store
        .append_event(
            &source.id,
            1,
            &AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
        )
        .unwrap();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| state.fork_thread(&source.id, cx));
    cx.run_until_parked();

    state.update(cx, |state, _cx| {
        let active = state.residents.active.as_ref().unwrap();
        let fork = &active.meta;
        assert_ne!(fork.id, source.id);
        assert!(fork.pending_fork);
        assert_eq!(
            fork.resume_cursor.as_ref().unwrap().0["thread_id"],
            "native-source"
        );
        assert_eq!(fork.cwd, source.cwd);
        assert_eq!(fork.worktree, None);
        assert!(!active.timeline.turn_running);
        assert_eq!(state.store.read_events(&fork.id).len(), 1);
        assert_eq!(
            std::fs::read(root.join(format!("{}.jsonl", fork.id))).unwrap(),
            std::fs::read(root.join(format!("{}.jsonl", source.id))).unwrap()
        );
    });
}

#[test]
fn store_writer_appends_events_in_fifo_order() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-writer-events");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store.clone()));

    state.host_update(cx, |state, cx| {
        state.record_event("ordered", &persisted_assistant_event("first"), cx);
        state.record_event("ordered", &persisted_assistant_event("second"), cx);
    });
    cx.run_until_parked();

    let events = store.read_events("ordered");
    let texts: Vec<_> = events
        .iter()
        .filter_map(|stored| match &stored.event {
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text },
                ..
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["first", "second"]);
}

#[test]
fn store_writer_upsert_is_visible_to_fresh_store() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-writer-upsert");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/upsert"), None);
    meta.title = "persisted by writer".into();
    let id = meta.id.clone();

    state.host_update(cx, |state, cx| state.persist_meta(&meta, cx));
    cx.run_until_parked();

    let fresh = SessionStore::open_at(root.clone()).unwrap();
    assert_eq!(
        fresh
            .load_index()
            .into_iter()
            .find(|stored| stored.id == id)
            .unwrap()
            .title,
        "persisted by writer"
    );
}

#[test]
fn store_writer_profile_secret_is_visible_to_fresh_store() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-writer-secret");
    let root = test_store.root().clone();
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        state.set_profile_secret(
            "klaude-kode",
            "ANTHROPIC_API_KEY",
            Some("writer-secret"),
            cx,
        );
    });
    cx.run_until_parked();

    let fresh = SettingsStore::new(root.clone());
    assert_eq!(
        fresh
            .profile_secrets("klaude-kode")
            .get("ANTHROPIC_API_KEY")
            .map(String::as_str),
        Some("writer-secret")
    );
}

#[test]
fn terminal_open_installs_after_executor_pump_and_preserves_cwd_override() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-terminal-open");
    let root = test_store.root().clone();
    let override_cwd = root.join("override");
    std::fs::create_dir_all(&override_cwd).unwrap();
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.update(cx, |state, _| {
        state.residents.active = Some(AppState::build_draft_session(
            "terminal-project".into(),
            root.clone(),
            ProviderKind::Codex,
            None,
            None,
            Vec::new(),
        ));
    });
    term::Terminal::with_spawn_cwd(override_cwd.clone(), || {
        state.host_update(cx, |state, cx| state.open_terminal_panel(cx));
    });
    state.read_with(cx, |state, _| {
        assert!(
            state
                .residents
                .active
                .as_ref()
                .unwrap()
                .terminal_workspace
                .terminals
                .is_empty()
        );
    });

    cx.run_until_parked();

    state.read_with(cx, |state, _| {
        let workspace = &state.residents.active.as_ref().unwrap().terminal_workspace;
        assert_eq!(workspace.terminals.len(), 1);
        assert!(state.terminal_panel_open());
        assert_eq!(workspace.terminals[0].terminal.cwd(), override_cwd);
    });
}

/// An `ActiveSession` wired to a fake live provider: commands land on the
/// returned receiver, nothing real is spawned.
fn fake_live_session(cwd: PathBuf) -> (ActiveSession, smol::channel::Receiver<SessionCommand>) {
    let (commands, receiver) = smol::channel::unbounded();
    let mut session = AppState::build_draft_session(
        "proj-t3".into(),
        cwd,
        ProviderKind::ClaudeCode,
        None,
        None,
        Vec::new(),
    );
    session.draft = false;
    session.runtime = Runtime::Live(commands);
    // What `ensure_started` records at launch — without these, `send_turn`
    // sees a live-config mismatch and restarts the provider instead of
    // dispatching.
    session.live_model = session.meta.model.clone();
    session.live_approval_mode = Some(session.meta.approval_mode);
    session.live_option_selections = session.meta.option_selections.clone();
    (session, receiver)
}

fn persisted_assistant_event(text: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: format!("item-{text}"),
        parent_item_id: None,
        content: ItemContent::AssistantMessage { text: text.into() },
    })
}

#[test]
fn cold_select_installs_immediately_then_loads_persisted_timeline() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-cold-select-async-test");
    let store = (*test_store).clone();
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/cold"), None);
    store.upsert_meta(&meta).unwrap();
    store
        .append_event(
            &meta.id,
            1,
            &persisted_assistant_event("persisted cold output"),
        )
        .unwrap();
    let id = meta.id.clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        state.select_session(&id, cx);
        let active = state.residents.active.as_ref().unwrap();
        assert_eq!(active.meta.id, id);
        assert!(active.timeline.entries.is_empty());
    });

    cx.run_until_parked();

    state.update(cx, |state, _| {
        assert!(state.residents.active.as_ref().unwrap().timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "persisted cold output")
        ));
    });
}

#[test]
fn parked_readopt_refolds_events_appended_while_parked() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-parked-readopt-async-test");
    let store = (*test_store).clone();
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/parked"), None);
    store.upsert_meta(&meta).unwrap();
    store
        .append_event(&meta.id, 1, &persisted_assistant_event("before parking"))
        .unwrap();
    let id = meta.id.clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| state.select_session(&id, cx));
    cx.run_until_parked();
    state.host_update(cx, |state, cx| {
        let active = state.residents.active.as_mut().unwrap();
        active.turn_in_flight = true;
        active.runtime = Runtime::Starting { generation: 1 };
        state.start_draft("other".into(), PathBuf::from("/tmp/other"), cx);
        state
            .store
            .append_event(&id, 2, &persisted_assistant_event("while parked"))
            .unwrap();
        state.select_session(&id, cx);
    });

    cx.run_until_parked();

    state.update(cx, |state, _| {
        assert!(state.residents.active.as_ref().unwrap().timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "while parked")
        ));
    });
}

#[test]
fn stale_timeline_completion_cannot_land_on_another_session() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-stale-timeline-load-test");
    let store = (*test_store).clone();
    let a = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/a"), None);
    let b = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/b"), None);
    store.upsert_meta(&a).unwrap();
    store.upsert_meta(&b).unwrap();
    store
        .append_event(&a.id, 1, &persisted_assistant_event("only session A"))
        .unwrap();
    store
        .append_event(&b.id, 1, &persisted_assistant_event("only session B"))
        .unwrap();
    let (id_a, id_b) = (a.id.clone(), b.id.clone());
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        state.select_session(&id_a, cx);
        state.select_session(&id_b, cx);
        assert_eq!(state.active_session_id(), Some(id_b.as_str()));
    });

    cx.run_until_parked();

    state.update(cx, |state, _| {
        let active = state.residents.active.as_ref().unwrap();
        assert_eq!(active.meta.id, id_b);
        assert!(active.timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "only session B")
        ));
        assert!(!active.timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "only session A")
        ));
    });
}

#[test]
fn timeline_load_retries_when_append_watermark_moves() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-timeline-watermark-test");
    let store = (*test_store).clone();
    let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp/watermark"), None);
    store.upsert_meta(&meta).unwrap();
    store
        .append_event(&meta.id, 1, &persisted_assistant_event("before load"))
        .unwrap();
    let id = meta.id.clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        state.select_session(&id, cx);
        state.record_event(&id, &persisted_assistant_event("raced append"), cx);
    });

    cx.run_until_parked();

    state.update(cx, |state, _| {
        let timeline = &state.residents.active.as_ref().unwrap().timeline;
        assert!(timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "before load")
        ));
        assert!(timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "raced append")
        ));
    });
}

/// The T3 Code regression this app must not inherit: send a message, hit
/// stop, get an error, then immediately open a new thread and send — and the
/// new thread's FIRST user message must be in its timeline (T3 loses the
/// bubble while the turn keeps working underneath).
///
/// The guarantees this pins: a message is folded into the timeline at the
/// moment it is dispatched (not asynchronously after), the fold only accepts
/// events whose session id matches the active session, and the interrupted
/// session's error cannot leak into the new thread.
#[test]
fn stop_then_new_thread_keeps_the_first_message_visible() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-t3-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-t3-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    // Session A, live (fake provider: commands land on `commands_a`).
    let (session, commands_a) = fake_live_session(cwd.clone());
    let (commands_b, receiver_b) = smol::channel::unbounded();
    let mut id_b = String::new();

    state.host_update(cx, |state, cx| {
        // No real provider may spawn if a start slips through.
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .binary_path = Some("/nonexistent/tcode-test-claude".into());

        // Send → the provider command is queued, then the adapter's
        // acceptance commits the user bubble.
        state.residents.active = Some(session);
        state.send_turn("first message".into(), Vec::new(), cx);
        let id_a = state.residents.active.as_ref().unwrap().meta.id.clone();
        let first_delivery = match commands_a.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected first SendTurn, got {other:?}"),
        };
        state.on_event(
            &id_a,
            AgentEvent::TurnAccepted {
                delivery_id: first_delivery,
            },
            cx,
        );
        assert!(state.residents.active.as_ref().unwrap().timeline.entries.iter().any(
            |entry| matches!(&entry.content, EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "first message")
        ));

        state.on_event(
            &id_a,
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            cx,
        );

        // Stop. The provider reports an error + an interrupted turn — the
        // truncated-error moment in the T3 repro.
        state.interrupt(cx);
        assert!(matches!(
            commands_a.try_recv(),
            Ok(SessionCommand::Interrupt)
        ));
        state.on_event(
            &id_a,
            AgentEvent::Error {
                message: "Request was aborted\nwith a second line the toast never showed"
                    .into(),
                fatal: false,
            },
            cx,
        );
        state.on_event(
            &id_a,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Interrupted,
                usage: None,
            },
            cx,
        );

        // Immediately: new thread, send. The draft commits to a NEW session;
        // the message waits in the queue while the provider starts (still
        // visible in the queue strip — never dropped).
        state.start_draft("proj-t3".into(), cwd.clone(), cx);
        state.send_turn("second message".into(), Vec::new(), cx);
        let active = state.residents.active.as_ref().unwrap();
        id_b = active.meta.id.clone();
        assert_ne!(id_a, id_b);
        assert_eq!(active.queue.len(), 1);

        // Provider comes up (simulated — the queue flush on start).
        state.residents.active.as_mut().unwrap().runtime = Runtime::Live(commands_b);
        assert_eq!(state.dispatch_next_queued(cx), Ok(true));
        let second_delivery = match receiver_b.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected second SendTurn, got {other:?}"),
        };
        state.on_event(
            &id_b,
            AgentEvent::TurnAccepted {
                delivery_id: second_delivery,
            },
            cx,
        );

        // THE assertion: the new thread's first message is a visible user
        // entry in a rendered turn, and session A's error did not leak in.
        let active = state.residents.active.as_ref().unwrap();
        let users: Vec<&str> = active
            .timeline
            .entries
            .iter()
            .filter_map(|e| match &e.content {
                EntryContent::Item(ItemContent::UserMessage { text, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec!["second message"]);
        let entry_turn = active.timeline.entries[0].turn;
        assert!(
            entry_turn < active.timeline.turns.len(),
            "user entry must belong to a rendered turn (turn {entry_turn} of {})",
            active.timeline.turns.len()
        );
        assert!(
            !active
                .timeline
                .entries
                .iter()
                .any(|e| matches!(e.content, EntryContent::Error { .. })),
            "session A's interrupt error leaked into the new thread"
        );
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        // And it is durable: a replay of the JSONL shows the same thing.
        let replayed = Timeline::fold_events(state.store.read_events(&id_b));
        assert!(replayed.entries.iter().any(
            |e| matches!(&e.content, EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "second message")
        ));
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn submitted_queue_head_cannot_leak_delivery_after_turn_completion() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-submitted-drop-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-submitted-drop-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (session, commands) = fake_live_session(cwd.clone());
    let id = session.meta.id.clone();

    state.host_update(cx, |state, cx| {
        state.store.upsert_meta(&session.meta).unwrap();
        state.sessions = state.store.load_index();
        state.residents.active = Some(session);
        state.send_turn("finish this task".into(), Vec::new(), cx);
        let delivery_id = match commands.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected SendTurn, got {other:?}"),
        };

        // The submitted head remains in the visible queue strip until its
        // provider acknowledgement. Its ✕ must not invalidate correlation.
        state.drop_queued(delivery_id, cx);
        state.on_event(&id, AgentEvent::TurnAccepted { delivery_id }, cx);
        state.on_event(
            &id,
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            cx,
        );
        state.on_event(
            &id,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(!state.turn_running_for(&id));

        state.start_draft("proj".into(), cwd.clone(), cx);
        state.select_session(&id, cx);
        assert!(!state.turn_running_for(&id));
        state.start_draft("proj".into(), cwd.clone(), cx);
        assert!(
            !state.turn_running_for(&id),
            "the completed delivery became Working again after reparking"
        );
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

/// The T-"stuck Working" family: an adapter whose event stream dies without
/// a `SessionClosed` must not leave the lifecycle flags set forever. The
/// pump synthesizes the close, which runs the ordinary teardown.
#[test]
fn dead_event_stream_without_close_clears_working_flags() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-dead-stream-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, _receiver) = smol::channel::unbounded();

    let id = state.host_update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::ClaudeCode, commands.clone());
        active.turn_in_flight = true;
        active.background_task_count = 2;
        let id = active.meta.id.clone();
        state.store.upsert_meta(&active.meta).unwrap();
        state.sessions = state.store.load_index();
        state.residents.active = Some(active);
        assert!(state.turn_running_for(&id));

        state.on_event_stream_ended(&id, &commands, cx);

        assert!(
            !state.turn_running_for(&id),
            "a dead event stream must not pin the session at Working"
        );
        let active = state.residents.active.as_ref().unwrap();
        assert!(matches!(active.runtime, Runtime::Idle));
        id
    });
    cx.run_until_parked();
    state.update(cx, |state, _| {
        // The synthesized close is durable evidence in the session log.
        let replayed = state.store.read_events(&id);
        assert!(
            replayed
                .iter()
                .any(|stored| matches!(stored.event, AgentEvent::SessionClosed { .. })),
            "the synthesized SessionClosed must be persisted"
        );
    });
}

/// Same leak, parked variant: the flags of a backgrounded session must
/// reset too (and the dead resident entry is released).
#[test]
fn dead_event_stream_clears_parked_working_flags() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-dead-parked-stream-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (commands, _receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut parked = live_session(ProviderKind::ClaudeCode, commands.clone());
        parked.turn_in_flight = true;
        parked.background_task_count = 1;
        let id = parked.meta.id.clone();
        state.store.upsert_meta(&parked.meta).unwrap();
        state.sessions = state.store.load_index();
        state.residents.parked.insert(id.clone(), parked);
        assert!(state.turn_running_for(&id));

        state.on_event_stream_ended(&id, &commands, cx);

        assert!(
            !state.turn_running_for(&id),
            "a dead event stream must not pin a parked session at Working"
        );
    });
}

/// A stale pump (the session was already closed, restarted, or handed to a
/// new provider process) must not tear down the successor runtime when its
/// old event channel drains.
#[test]
fn stale_pump_close_leaves_successor_runtime_alone() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-stale-pump-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let (old_commands, _old_receiver) = smol::channel::unbounded();
    let (new_commands, _new_receiver) = smol::channel::unbounded();

    state.host_update(cx, |state, cx| {
        let mut active = live_session(ProviderKind::ClaudeCode, new_commands);
        active.turn_in_flight = true;
        let id = active.meta.id.clone();
        state.store.upsert_meta(&active.meta).unwrap();
        state.sessions = state.store.load_index();
        state.residents.active = Some(active);

        // The old pump drains after the session moved to a new provider.
        state.on_event_stream_ended(&id, &old_commands, cx);

        let active = state.residents.active.as_ref().unwrap();
        assert!(
            matches!(active.runtime, Runtime::Live(_)),
            "a stale pump must not tear down the successor provider"
        );
        assert!(active.turn_in_flight);
        assert!(state.turn_running_for(&id));

        // And an idle session ignores stream-end noise entirely.
        state.residents.active.as_mut().unwrap().runtime = Runtime::Idle;
        state.residents.active.as_mut().unwrap().turn_in_flight = false;
        state.on_event_stream_ended(&id, &old_commands, cx);
        assert!(!state.turn_running_for(&id));
    });
}

#[test]
fn turn_running_for_is_independent_of_active_or_parked_location() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-working-location-test");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let commands = smol::channel::unbounded().0;

    let mut idle = live_session(ProviderKind::ClaudeCode, commands.clone());
    idle.meta.id = "idle".into();

    let mut turn = live_session(ProviderKind::ClaudeCode, commands.clone());
    turn.meta.id = "turn".into();
    turn.turn_in_flight = true;

    let mut delivery = live_session(ProviderKind::ClaudeCode, commands.clone());
    delivery.meta.id = "delivery".into();
    delivery.delivery_in_flight = Some(7);

    let mut queued = live_session(ProviderKind::ClaudeCode, commands.clone());
    queued.meta.id = "queued".into();
    queued.push_queued("waiting".into(), Vec::new());

    let mut background = live_session(ProviderKind::ClaudeCode, commands.clone());
    background.meta.id = "background".into();
    background.background_task_count = 1;

    let mut stale_timeline = live_session(ProviderKind::ClaudeCode, commands);
    stale_timeline.meta.id = "stale-timeline".into();
    stale_timeline.timeline.apply_at(
        None,
        &AgentEvent::TurnStarted {
            turn_id: "stale".into(),
        },
    );

    state.update(cx, |state, _| {
        for (label, session, expected) in [
            ("idle", idle, false),
            ("turn", turn, true),
            ("delivery", delivery, true),
            ("queued", queued, true),
            ("background", background, true),
            ("stale timeline", stale_timeline, false),
        ] {
            let id = session.meta.id.clone();
            state.residents.active = Some(session);
            let active_answer = state.turn_running_for(&id);

            let parked = state.residents.active.take().unwrap();
            state.residents.parked.insert(id.clone(), parked);
            let parked_answer = state.turn_running_for(&id);

            assert_eq!(
                active_answer, parked_answer,
                "{label} changed answer when moved between active and parked"
            );
            assert_eq!(active_answer, expected, "{label} work predicate");
            state.residents.parked.remove(&id);
        }
    });
}

/// The T3 Code session-reaper failure class, our variant: switching to
/// another thread must NOT kill a session whose turn is still running. The
/// session parks in the background — process and queue alive, events still
/// recorded, sidebar still "Working" — and selecting it again re-adopts it
/// with the streamed-while-parked content visible.
#[test]
fn switching_threads_parks_a_working_session_instead_of_killing_it() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-park-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-park-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    // A live session (fake provider: commands land on `commands_a`).
    let (session, commands_a) = fake_live_session(cwd.clone());
    let id_a = session.meta.id.clone();

    state.host_update(cx, |state, cx| {
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .binary_path = Some("/nonexistent/tcode-test-claude".into());

        // A live session with a running turn (the overnight workflow).
        state.store.upsert_meta(&session.meta).unwrap();
        state.sessions = state.store.load_index();
        state.residents.active = Some(session);
        state.send_turn("run the long migration".into(), Vec::new(), cx);
        state.send_turn("queued follow-up".into(), Vec::new(), cx);
        let first_delivery = match commands_a.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected migration SendTurn, got {other:?}"),
        };
        state.on_event(
            &id_a,
            AgentEvent::TurnAccepted {
                delivery_id: first_delivery,
            },
            cx,
        );
        state.on_event(
            &id_a,
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            cx,
        );

        // Glance at another thread: the session must survive, not die.
        state.start_draft("proj-t3".into(), cwd.clone(), cx);
        assert!(
            commands_a.try_recv().is_err(),
            "switching threads must not send Shutdown to a working session"
        );
        assert!(
            state.turn_running_for(&id_a),
            "a parked working session keeps its sidebar Working status"
        );

        // The parked session keeps streaming; its events keep landing in
        // the JSONL even though another thread is on screen.
        state.on_event(
            &id_a,
            AgentEvent::ItemCompleted(ThreadItem {
                id: "bg-1".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "Migration step 1 done.".into(),
                },
            }),
            cx,
        );

        // Its turn completes in the background → the queued follow-up goes
        // out as the next turn, on the same process.
        state.on_event(
            &id_a,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        let follow_up_delivery = match commands_a.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected follow-up SendTurn, got {other:?}"),
        };
        state.on_event(
            &id_a,
            AgentEvent::TurnAccepted {
                delivery_id: follow_up_delivery,
            },
            cx,
        );
        assert!(state.turn_running_for(&id_a));

        // Coming back re-adopts the live session: everything that happened
        // while parked is in the timeline, and the turn is still running.
        state.select_session(&id_a, cx);
    });

    cx.run_until_parked();

    state.host_update(cx, |state, cx| {
        let active = state.residents.active.as_ref().unwrap();
        assert_eq!(active.meta.id, id_a);
        assert!(matches!(active.runtime, Runtime::Live(_)));
        assert!(active.turn_in_flight);
        assert!(active.timeline.entries.iter().any(|e| matches!(
            &e.content,
            EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "Migration step 1 done."
        )));
        assert!(active.timeline.entries.iter().any(|e| matches!(
            &e.content,
            EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "queued follow-up"
        )));

        // The second turn completes with nothing queued: NOW the provider
        // shuts down — work finished, not reaped.
        state.on_event(
            &id_a,
            AgentEvent::TurnCompleted {
                turn_id: "turn-2".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(
            !state.turn_running_for(&id_a),
            "a completed active session must not remain Working"
        );
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

/// A parked session that runs out of work becomes an idle resident instead
/// of immediately rebuilding its provider on the next selection.
#[test]
fn drained_parked_session_stays_resident() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-parkend-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-parkend-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    // A live session (fake provider: commands land on `commands`).
    let (session, commands) = fake_live_session(cwd.clone());
    let id = session.meta.id.clone();

    state.host_update(cx, |state, cx| {
        state.store.upsert_meta(&session.meta).unwrap();
        state.sessions = state.store.load_index();
        state.residents.active = Some(session);
        state.send_turn("one last thing".into(), Vec::new(), cx);
        let delivery_id = match commands.try_recv() {
            Ok(SessionCommand::SendTurn { delivery_id, .. }) => delivery_id,
            other => panic!("expected final SendTurn, got {other:?}"),
        };
        state.on_event(&id, AgentEvent::TurnAccepted { delivery_id }, cx);

        state.start_draft("proj".into(), cwd.clone(), cx);
        assert!(state.turn_running_for(&id));

        // The parked turn finishes with an empty queue → resident grace.
        state.on_event(
            &id,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            cx,
        );
        assert!(commands.try_recv().is_err(), "drain sent Shutdown");
        assert!(state.residents.parked.contains_key(&id));
        assert!(state.residents.parked[&id].idle_since.is_some());
        assert!(!state.turn_running_for(&id));
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

/// A failed provider start must not destroy what the user typed: the queued
/// message stays in the queue (visible in the strip, flushed by the next
/// successful start) instead of being cleared.
#[test]
fn failed_provider_start_keeps_the_queued_message() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-t3f-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-t3f-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        // A binary that cannot exist → start_session fails fast.
        state
            .settings
            .provider_mut(ProviderKind::ClaudeCode)
            .binary_path = Some("/nonexistent/tcode-test-claude".into());
        state.start_draft("proj-fail".into(), cwd.clone(), cx);
        state.send_turn("do not lose me".into(), Vec::new(), cx);
        assert_eq!(state.residents.active.as_ref().unwrap().queue.len(), 1);
    });

    // Let the spawned start attempt run to its failure.
    cx.run_until_parked();

    state.update(cx, |state, _| {
        let active = state.residents.active.as_ref().unwrap();
        assert!(
            matches!(active.runtime, Runtime::Idle),
            "failed start must return to Idle"
        );
        assert_eq!(
            active.queue.first().map(|m| m.text.as_str()),
            Some("do not lose me"),
            "the user's text must survive a failed provider start"
        );
        // The failure itself is on the record.
        assert!(
            active
                .timeline
                .entries
                .iter()
                .any(|e| matches!(e.content, EntryContent::ProviderStartError { .. })),
            "the start failure must be recorded in the timeline"
        );
    });

    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn plan_workspace_save_completes_after_background_executor_runs() {
    let cx = &mut TestAppContext::default();
    let cwd = std::env::temp_dir().join(format!("tcode-plan-save-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let test_store = TestStore::new("tcode-plan-save-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));

    state.host_update(cx, |state, cx| {
        state.start_draft("plan-project".into(), cwd.clone(), cx);
        state.save_plan_to_workspace("# Saved plan".into(), cx);
        // No "not written yet" assertion here: the write runs on the global
        // smol pool, whose threads are concurrent with this update, so any
        // file-existence check races them (flaked on fast Windows runners
        // both inside and after this update).
    });

    // The write lands on a real blocking thread that run_until_parked does
    // not wait for; poll with a bounded budget.
    let mut contents = None;
    for _ in 0..500 {
        cx.run_until_parked();
        if let Ok(text) = std::fs::read_to_string(cwd.join("PLAN-1.md")) {
            contents = Some(text);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(contents.as_deref(), Some("# Saved plan"));
    let _ = std::fs::remove_dir_all(&cwd);
}

/// Dispatch replies arrive from a real blocking thread, which
/// `run_until_parked` does not wait for; poll with a bounded budget.
fn recv_dispatch_reply<T>(cx: &mut TestAppContext, rx: &smol::channel::Receiver<T>) -> T {
    for _ in 0..500 {
        cx.run_until_parked();
        if let Ok(reply) = rx.try_recv() {
            return reply;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("dispatch reply did not arrive within the polling budget");
}

#[test]
fn orchestrate_dispatch_resolves_cwd_before_reply() {
    let cx = &mut TestAppContext::default();
    let root = std::env::temp_dir().join(format!("tcode-dispatch-cwd-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let test_store = TestStore::new("tcode-dispatch-cwd-data");
    let store = (*test_store).clone();
    let state = cx.new_entity(|_| AppState::new(store));
    let parent = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
    let parent_id = parent.id.clone();
    let missing = root.join("missing");
    let (reply, response) = smol::channel::bounded(1);

    state.host_update(cx, |state, cx| {
        state.sessions.push(parent);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Dispatch {
                parent_id,
                provider: "codex".into(),
                model: Some("gpt-5.6-sol".into()),
                effort: None,
                profile: None,
                access: None,
                title: "Child".into(),
                brief: "Inspect the workspace".into(),
                cwd: Some(missing.to_string_lossy().into_owned()),
                worktree: None,
                archive_on_complete: None,
                result_max_chars: None,
            },
            reply,
            cx,
        );
    });
    assert!(
        response.try_recv().is_err(),
        "cwd resolution must not reply from the GPUI update"
    );

    assert_eq!(
        recv_dispatch_reply(cx, &response).unwrap_err(),
        format!("invalid cwd: {}", missing.display())
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn orchestrate_worktree_dispatch_resolves_child_cwd_to_worktree() {
    let cx = &mut TestAppContext::default();
    let root =
        std::env::temp_dir().join(format!("tcode-dispatch-worktree-{}", uuid::Uuid::new_v4()));
    let isolated_worktrees = root.join("tcode-owned-worktrees");
    // The services lifecycle honors this process-local override so this test
    // cannot create or clean entries in the user's real ~/.tcode/worktrees.
    unsafe {
        std::env::set_var("TCODE_WORKTREES_DIR", &isolated_worktrees);
    }
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-b", "main"]).unwrap();
    run_git(&root, &["config", "user.name", "tcode"]).unwrap();
    run_git(&root, &["config", "user.email", "tcode@localhost"]).unwrap();
    std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
    run_git(&root, &["add", "tracked.txt"]).unwrap();
    run_git(&root, &["commit", "-m", "initial"]).unwrap();

    let test_store = TestStore::new("tcode-dispatch-worktree-data");
    let state = cx.new_entity(|_| AppState::new((*test_store).clone()));
    let parent = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
    let parent_id = parent.id.clone();
    let (reply, response) = smol::channel::bounded(1);

    state.host_update(cx, |state, cx| {
        state.sessions.push(parent);
        state.handle_orchestrate_op(
            orchestrate_mcp::OrchestrateOp::Dispatch {
                parent_id,
                provider: "codex".into(),
                model: Some("gpt-5.6-sol".into()),
                effort: None,
                profile: None,
                access: None,
                title: "Isolated child".into(),
                brief: "Inspect the workspace".into(),
                cwd: None,
                worktree: Some(true),
                archive_on_complete: None,
                result_max_chars: None,
            },
            reply,
            cx,
        );
    });
    let response = recv_dispatch_reply(cx, &response).unwrap();
    let child_id = response["thread_id"].as_str().unwrap().to_string();
    let expected_branch = format!("tcode/{child_id}");
    let expected_path = isolated_worktrees.join(&child_id);
    assert_eq!(
        response["worktree_path"],
        expected_path.display().to_string(),
        "dispatch response: {response}"
    );
    assert_eq!(response["worktree_branch"], expected_branch);
    state.update(cx, |state, _| {
        let child = state.find_meta(&child_id).unwrap();
        assert_eq!(child.cwd, expected_path);
        assert_eq!(
            child
                .worktree
                .as_ref()
                .map(|worktree| worktree.branch.as_str()),
            Some(expected_branch.as_str())
        );
    });
    remove_git_worktree(&root, &expected_path).unwrap();
    unsafe {
        std::env::remove_var("TCODE_WORKTREES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
}
