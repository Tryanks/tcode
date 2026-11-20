use super::*;

const AGENT: &str = "custom:agy";
const OTHER: &str = "custom:other";

fn installed(id: &str) -> InstalledAgent {
    InstalledAgent {
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
    }
}

fn select(
    id: &str,
    role: agent::OptionRole,
    values: &[&str],
    default: &str,
) -> agent::OptionDescriptor {
    agent::OptionDescriptor::Select {
        id: id.into(),
        label: id.into(),
        options: values
            .iter()
            .map(|value| agent::SelectOption {
                value: (*value).into(),
                label: (*value).into(),
                description: None,
                unavailable: None,
                speed: None,
                summary: None,
                approval: agent::approval_class(ProviderKind::Acp, id, value),
            })
            .collect(),
        default_value: Some(default.into()),
        role,
        apply: agent::ApplyTiming::Live,
        recommended: None,
        permissive: None,
        minimum: None,
    }
}

/// What `AGENT`'s probe saw: two models, the first its default, and a mode
/// and a thought level, each at the value a session starts at.
fn catalog() -> agent::AcpCatalog {
    let model = |id: &str, is_default| agent::ModelSpec {
        id: id.into(),
        display_name: id.to_uppercase(),
        is_default,
        options: Vec::new(),
    };
    agent::AcpCatalog {
        models: vec![model("model-a", true), model("model-b", false)],
        options: vec![
            select(
                agent::ACP_MODE_OPTION_ID,
                agent::OptionRole::Permission,
                &["default", "yolo"],
                "default",
            ),
            select(
                agent::EFFORT_OPTION_ID,
                agent::OptionRole::Effort,
                &["low", "high"],
                "low",
            ),
        ],
    }
}

/// A data directory with `AGENT` and `OTHER` installed, `AGENT`'s catalog
/// probed, and one project.
fn acp_store(prefix: &str) -> (TestStore, Project) {
    let store = TestStore::new(prefix);
    let project = Project::from_root(store.root().join("project"));
    fs::create_dir_all(&project.root).unwrap();
    store.upsert_project(&project).unwrap();
    let mut settings = Settings::default();
    for id in [AGENT, OTHER] {
        settings.acp_agents.insert(id.into(), installed(id));
    }
    tcode_services::settings::SettingsStore::new(store.root().clone())
        .save(&settings)
        .unwrap();
    store.save_acp_catalog(AGENT, &catalog()).unwrap();
    (store, project)
}

fn new_draft(state: &TestEntity, cx: &mut TestAppContext, project: &Project) -> String {
    let CommandResponse::SessionId(Some(draft)) = state
        .command(
            cx,
            Command::StartDraft {
                project_id: project.id.clone(),
                cwd: project.root.clone(),
            },
        )
        .unwrap()
    else {
        panic!("expected a draft");
    };
    state.open(cx, &draft);
    draft
}

fn choose_agent(id: &str, agent: &str, model: Option<&str>) -> Command {
    Command::SetActiveAcpAgent {
        session_id: id.into(),
        id: agent.into(),
        model: model.map(Into::into),
    }
}

fn value<'a>(selections: &'a [OptionSelection], id: &str) -> Option<&'a serde_json::Value> {
    selections
        .iter()
        .find(|selection| selection.id == id)
        .map(|selection| &selection.value)
}

fn roles(status: &SessionStatus) -> Vec<(&str, agent::OptionRole)> {
    status
        .provider_option_descriptors
        .iter()
        .map(|descriptor| match descriptor {
            agent::OptionDescriptor::Select { id, role, .. }
            | agent::OptionDescriptor::Boolean { id, role, .. } => (id.as_str(), *role),
        })
        .collect()
}

/// Before its first message, a draft on an ACP agent runs on what the agent
/// offers a new session: its default model, and its options at their
/// starting values, the mode among them as the permission control. What the
/// user then chooses is the draft's, and another of the agent's models keeps
/// the options chosen.
#[test]
fn an_acp_draft_starts_on_the_agents_catalog_and_keeps_what_the_user_chose() {
    let cx = &mut TestAppContext::default();
    let (store, project) = acp_store("tcode-acp-draft-catalog");
    let (state, _providers) = played_host(cx, &store);
    let draft = new_draft(&state, cx, &project);

    state
        .command(cx, choose_agent(&draft, AGENT, None))
        .unwrap();
    let status = cx.seen().status(&draft);
    assert_eq!(status.acp_agent_id.as_deref(), Some(AGENT));
    assert_eq!(status.requested_model.as_deref(), Some("model-a"));
    assert_eq!(
        roles(status),
        [
            (agent::ACP_MODE_OPTION_ID, agent::OptionRole::Permission),
            (agent::EFFORT_OPTION_ID, agent::OptionRole::Effort),
        ]
    );
    assert_eq!(
        value(
            &status.provider_option_selections,
            agent::ACP_MODE_OPTION_ID
        ),
        Some(&serde_json::json!("default"))
    );
    assert!(status.provider_option_requested_selections.is_empty());

    state
        .command(
            cx,
            Command::SetActiveOption {
                session_id: draft.clone(),
                id: agent::ACP_MODE_OPTION_ID.into(),
                value: Some(serde_json::json!("yolo")),
            },
        )
        .unwrap();
    state
        .command(cx, choose_agent(&draft, AGENT, Some("model-b")))
        .unwrap();
    let status = cx.seen().status(&draft);
    assert_eq!(status.requested_model.as_deref(), Some("model-b"));
    assert_eq!(
        status.provider_option_requested_selections,
        [OptionSelection {
            id: agent::ACP_MODE_OPTION_ID.into(),
            value: serde_json::json!("yolo"),
        }]
    );
    assert_eq!(roles(status).len(), 2);
}

/// A draft that follows a thread on the agent starts on that thread's
/// model, not the agent's default, and on the agent's own starting mode,
/// never the mode another thread runs in: an agent may switch a session to
/// a mode that asks nothing, and the thread follows it, but the agent's
/// catalog is only what its probe saw.
#[test]
fn a_mode_a_thread_runs_in_reaches_neither_the_catalog_nor_a_new_draft() {
    let cx = &mut TestAppContext::default();
    let (store, project) = acp_store("tcode-acp-live-mode");
    stored_thread(&store, "thread", ProviderKind::Acp, &project.root, |meta| {
        meta.project_id = Some(project.id.clone());
        meta.acp_agent_id = Some(AGENT.into());
        meta.model = Some("model-b".into());
    });
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Providers);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    provider.play([
        AgentEvent::ProviderOptions {
            descriptors: catalog().options,
            selections: vec![OptionSelection {
                id: agent::ACP_MODE_OPTION_ID.into(),
                value: serde_json::json!("yolo"),
            }],
        },
        completed_turn("turn"),
    ]);
    cx.run_until_seen(|view| view.status("thread").running_turn.is_none());
    // The thread follows the mode its agent switched to.
    assert_eq!(
        value(
            &cx.seen()
                .status("thread")
                .provider_option_requested_selections,
            agent::ACP_MODE_OPTION_ID
        ),
        Some(&serde_json::json!("yolo"))
    );

    let draft = new_draft(&state, cx, &project);
    let status = cx.seen().status(&draft);
    assert_eq!(status.acp_agent_id.as_deref(), Some(AGENT));
    assert_eq!(status.requested_model.as_deref(), Some("model-b"));
    assert_eq!(
        value(
            &status.provider_option_selections,
            agent::ACP_MODE_OPTION_ID
        ),
        Some(&serde_json::json!("default"))
    );
    assert_eq!(
        value(
            &status.provider_option_requested_selections,
            agent::ACP_MODE_OPTION_ID
        ),
        None
    );
    assert_eq!(store.load_acp_catalog(AGENT), Some(catalog()));
    assert_eq!(cx.seen().providers().acp_models[AGENT], catalog().models);
}

/// The model a space lets its members run is checked against the model the
/// thread would run, the agent's default included when none is named.
#[test]
fn a_space_refuses_an_acp_agents_default_model_it_does_not_allow() {
    let cx = &mut TestAppContext::default();
    let (store, project) = acp_store("tcode-acp-space-model");
    stored_thread(
        &store,
        "thread",
        ProviderKind::Codex,
        &project.root,
        |meta| {
            meta.project_id = Some(project.id.clone());
        },
    );
    let (state, _providers) = played_host(cx, &store);
    state.open(cx, "thread");
    let member = tcode_protocol::Principal::Space {
        policy_revision: 1,
        space_id: "s".into(),
        space_name: "Shared".into(),
        project_ids: vec![project.id.clone()],
        device_id: "phone".into(),
        device_name: "Phone".into(),
        policy: tcode_protocol::SpacePolicy {
            agents: tcode_protocol::AgentScope::Only(vec![tcode_protocol::AgentGrant {
                profile_id: Settings::builtin_profile_id(ProviderKind::Acp).into(),
                models: tcode_protocol::ModelGrant::Only(vec!["model-b".into()]),
                acp_agents: vec![AGENT.into()],
            }]),
            ..tcode_protocol::SpacePolicy::unrestricted()
        },
    };
    state
        .query_as(cx, member.clone(), tcode_protocol::Query::Ping)
        .unwrap();

    let refused = state
        .command_as(cx, member.clone(), choose_agent("thread", AGENT, None))
        .unwrap_err();
    assert_eq!(refused.code, "space_policy");
    assert_eq!(
        cx.seen().status("thread").provider,
        ProviderKind::Codex,
        "a refused choice changes nothing"
    );
    state
        .command_as(cx, member, choose_agent("thread", AGENT, Some("model-b")))
        .unwrap();
    let status = cx.seen().status("thread");
    assert_eq!(status.acp_agent_id.as_deref(), Some(AGENT));
    assert_eq!(status.requested_model.as_deref(), Some("model-b"));
}

/// Another model of the agent a thread runs is set where the agent runs,
/// before the next message, with the thread's options and without a new
/// process or a handover. Another agent cannot take up the session, so a
/// thread with history hands it over once the user confirms.
#[test]
fn another_model_of_the_agent_applies_live_and_another_agent_relays() {
    let cx = &mut TestAppContext::default();
    let (store, project) = acp_store("tcode-acp-model-switch");
    let effort = OptionSelection {
        id: agent::EFFORT_OPTION_ID.into(),
        value: serde_json::json!("high"),
    };
    stored_thread(&store, "thread", ProviderKind::Acp, &project.root, |meta| {
        meta.project_id = Some(project.id.clone());
        meta.acp_agent_id = Some(AGENT.into());
        meta.model = Some("model-a".into());
        meta.option_selections.push(effort.clone());
    });
    let (state, providers) = played_host(cx, &store);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    provider.play([completed_turn("turn")]);
    cx.run_until_seen(|view| view.status("thread").running_turn.is_none());

    state
        .command(cx, choose_agent("thread", AGENT, Some("model-b")))
        .unwrap();
    assert!(cx.seen().status("thread").relay_confirmation.is_none());
    state.send(cx, "thread", "next");
    match provider.next_command(cx) {
        SessionCommand::ApplySettings {
            model,
            option_selections,
        } => {
            assert_eq!(model.as_deref(), Some("model-b"));
            assert_eq!(option_selections, [effort]);
        }
        other => panic!("expected the model applied live, got {other:?}"),
    }
    provider.play([AgentEvent::SettingsApplied { applied: true }]);
    provider.start_turn(cx, "turn-2");
    provider.play([completed_turn("turn-2")]);
    cx.run_until_seen(|view| view.status("thread").running_turn.is_none());
    assert_eq!(providers.count(), 1);

    state
        .command(cx, choose_agent("thread", OTHER, None))
        .unwrap();
    let status = cx.seen().status("thread");
    assert_eq!(status.acp_agent_id.as_deref(), Some(OTHER));
    assert!(status.relay_confirmation.is_some());
}

/// A thread stored while an ACP agent's model was one of its options holds
/// the model it last ran as the `acp:model` selection: that is the model its
/// agent starts on, and it is no longer an option.
#[test]
fn a_stored_acp_model_selection_is_the_model_the_thread_starts_on() {
    let cx = &mut TestAppContext::default();
    let (store, project) = acp_store("tcode-acp-stored-model");
    let effort = OptionSelection {
        id: agent::EFFORT_OPTION_ID.into(),
        value: serde_json::json!("high"),
    };
    stored_thread(&store, "thread", ProviderKind::Acp, &project.root, |meta| {
        meta.project_id = Some(project.id.clone());
        meta.acp_agent_id = Some(AGENT.into());
        meta.model = Some("model-a".into());
        meta.option_selections = vec![
            OptionSelection {
                id: agent::ACP_MODEL_OPTION_ID.into(),
                value: serde_json::json!("model-b"),
            },
            effort.clone(),
        ];
    });
    let (state, providers) = played_host(cx, &store);
    let provider = first_turn(&state, cx, &providers, "thread", 0);
    assert_eq!(provider.options.model.as_deref(), Some("model-b"));
    assert_eq!(provider.options.option_selections, [effort]);
    assert_eq!(
        cx.seen().status("thread").requested_model.as_deref(),
        Some("model-b")
    );
}
