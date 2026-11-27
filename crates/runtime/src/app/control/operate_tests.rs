use super::*;
use crate::app::control::tests::{said, turn_completed};
use crate::app::next_command;
use crate::app::test_support::*;
use control_mcp::{ControlOp, GetThreadParams, MoveThreadParams, Workspace};
use tcode_protocol::TurnDue;

const MESSAGE: &str = "Find out why the sync test flakes";

/// Run one `tcode_control` call from `caller` the way the broker does, and wait for its answer.
fn call(
    state: &TestEntity,
    cx: &mut TestAppContext,
    caller: &str,
    op: ControlOp,
) -> Result<serde_json::Value, String> {
    let (reply, answer) = smol::channel::bounded(1);
    state.update(cx, |state, cx| {
        state.handle_control_call(caller.into(), op, None, reply, cx)
    });
    cx.run_until_recv(&answer)
}

/// A project at `root` that the store held before the host started.
fn stored_project(store: &TestStore, id: &str, root: &Path, edit: impl FnOnce(&mut Project)) {
    std::fs::create_dir_all(root).unwrap();
    let mut project = Project::from_root(root.to_path_buf());
    project.id = id.into();
    edit(&mut project);
    store.upsert_project(&project).unwrap();
}

/// A thread of `project` working in `cwd` that the store held before the host started.
fn stored(
    store: &TestStore,
    id: &str,
    provider: ProviderKind,
    project: &str,
    cwd: &Path,
    edit: impl FnOnce(&mut SessionMeta),
) {
    stored_thread(store, id, provider, cwd, |meta| {
        meta.project_id = Some(project.into());
        meta.title = format!("Thread {id}");
        edit(meta);
    });
}

fn start(project_id: &str) -> ControlOp {
    ControlOp::StartThread(StartThreadParams {
        project_id: Some(project_id.into()),
        cwd: None,
        workspace: None,
        subagent: None,
        provider: "codex".into(),
        acp_agent: None,
        model: None,
        profile: None,
        options: None,
        title: Some("Investigate the flaky test".into()),
        message: MESSAGE.into(),
        attachments: None,
        request_id: "start-1".into(),
    })
}

/// The threads the index lists as started by another thread.
fn started(cx: &TestAppContext) -> Vec<SessionMeta> {
    cx.seen()
        .sessions()
        .filter(|meta| meta.started_by.is_some())
        .cloned()
        .collect()
}

fn permission(meta: &SessionMeta) -> Option<&serde_json::Value> {
    meta.option_selections
        .iter()
        .find(|selection| selection.id == "permissions")
        .map(|selection| &selection.value)
}

/// `start_thread` makes one ordinary thread whose first message is the agent's, leaves the
/// user's standing draft alone, and a retry with the same request id — also after the host
/// restarts — answers with that thread instead of making another.
#[test]
fn start_thread_makes_one_ordinary_thread_across_retries_and_restarts() {
    let store = TestStore::new("tcode-control-start-thread");
    let root = store.root().join("project");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("notes.txt"), b"notes").unwrap();
    stored_project(&store, "a", &root, |project| {
        project
            .permission_defaults
            .insert("codex".into(), "read_only".into());
    });
    stored(
        &store,
        "caller",
        ProviderKind::ClaudeCode,
        "a",
        &root,
        |_| {},
    );
    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    state.open(cx, "caller");
    let draft = state
        .command(
            cx,
            Command::StartDraft {
                project_id: "a".into(),
                cwd: root.clone(),
            },
        )
        .unwrap();
    let CommandResponse::SessionId(Some(draft)) = draft else {
        panic!("unexpected answer: {draft:?}");
    };
    state.open(cx, &draft);

    // The model may not choose the permission mode, leave the project or attach a file the
    // user did not attach; nothing is made.
    let shot = cx.seen().status("caller").attachments_dir.join("shot.png");
    std::fs::create_dir_all(shot.parent().unwrap()).unwrap();
    std::fs::write(&shot, b"png").unwrap();
    let with = |edit: &dyn Fn(&mut StartThreadParams)| {
        let ControlOp::StartThread(mut params) = start("a") else {
            unreachable!()
        };
        edit(&mut params);
        ControlOp::StartThread(params)
    };
    let outside = std::env::temp_dir().display().to_string();
    let notes = root.join("notes.txt").display().to_string();
    for op in [
        with(&|p| {
            p.options = Some(
                [("permissions".to_owned(), serde_json::json!("full_access"))]
                    .into_iter()
                    .collect(),
            )
        }),
        with(&|p| p.cwd = Some("../".into())),
        with(&|p| p.cwd = Some(outside.clone())),
        with(&|p| p.attachments = Some(vec![notes.clone()])),
    ] {
        assert!(call(&state, cx, "caller", op).is_err());
    }
    cx.run_until_parked();
    assert!(started(cx).is_empty());

    // The new thread's provider is still starting, so its first message waits where a
    // client sees it.
    providers.hold();
    let first = call(
        &state,
        cx,
        "caller",
        with(&|p| {
            p.cwd = Some("sub".into());
            p.attachments = Some(vec![shot.display().to_string()]);
        }),
    )
    .unwrap();
    assert_eq!(first["created"], true);
    let id = first["thread_id"].as_str().unwrap().to_owned();
    state.open(cx, &id);
    let threads = started(cx);
    assert_eq!(threads.len(), 1);
    let meta = &threads[0];
    assert_eq!(meta.id, id);
    assert_eq!(
        meta.parent_session_id, None,
        "an ordinary thread, not a child"
    );
    assert_eq!(meta.title, "Investigate the flaky test");
    assert_eq!(meta.cwd, root.join("sub"));
    assert_eq!(
        permission(meta),
        Some(&serde_json::json!("read_only")),
        "the project's default permission"
    );
    let status = cx.seen().status(&id);
    assert!(!status.draft);
    assert_eq!(status.queued_messages[0].text, MESSAGE);
    let standing = cx.seen().status(&draft);
    assert!(standing.draft, "the user's draft stays a draft");
    assert!(standing.queued_messages.is_empty());

    let retry = call(&state, cx, "caller", start("a")).unwrap();
    assert_eq!(retry["created"], false);
    assert_eq!(retry["thread_id"], id.as_str());
    cx.run_until_parked();
    assert_eq!(started(cx).len(), 1);

    providers.let_go();
    let provider = providers.started(cx, 0);
    let SessionCommand::SendTurn {
        delivery_id,
        text,
        attachments,
        ..
    } = provider.next_command(cx)
    else {
        panic!("expected the first message to reach the provider");
    };
    assert_eq!(text, MESSAGE);
    assert_eq!(attachments.len(), 1);
    provider.play([AgentEvent::TurnAccepted { delivery_id }]);
    provider.fence(cx, &id);
    assert_eq!(said(cx, &id, MessageOrigin::Agent), [MESSAGE]);
    cx.run_until_parked();
    drop(state);

    let cx = &mut TestAppContext::default();
    let (restarted, _providers) = played_host(cx, &store);
    restarted.subscribe(cx, Topic::Index);
    let after_restart = call(&restarted, cx, "caller", start("a")).unwrap();
    assert_eq!(after_restart["created"], false);
    assert_eq!(after_restart["thread_id"], id.as_str());
    cx.run_until_parked();
    assert_eq!(started(cx).len(), 1);
}

/// `subagent: true` makes the thread the caller's child: it works in the caller's directory,
/// runs with the permission the child-approval setting chooses rather than the project's
/// default, carries the report contract and owes the caller a report. A worktree is only for a
/// subagent, and one that cannot be made starts no subagent at all.
#[test]
fn a_subagent_is_a_child_that_owes_its_caller_a_report() {
    use tcode_core::settings::ChildApprovalMode;
    use tcode_core::settlement::AgentReport;

    let store = TestStore::new("tcode-control-start-subagent");
    let root = store.root().join("project");
    stored_project(&store, "a", &root, |project| {
        project
            .permission_defaults
            .insert("codex".into(), "read_only".into());
    });
    let cwd = root.join("sub");
    stored(
        &store,
        "caller",
        ProviderKind::ClaudeCode,
        "a",
        &cwd,
        |_| {},
    );
    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    let set = |cx: &mut TestAppContext, patch: SettingsPatch| {
        state.command(cx, Command::PatchSettings { patch }).unwrap();
    };
    let with = |request_id: &str, edit: &dyn Fn(&mut StartThreadParams)| {
        let ControlOp::StartThread(mut params) = start("a") else {
            unreachable!()
        };
        params.request_id = request_id.into();
        params.subagent = Some(true);
        edit(&mut params);
        ControlOp::StartThread(params)
    };

    let ordinary_worktree = with("ordinary-worktree", &|p| {
        p.subagent = None;
        p.workspace = Some(Workspace::Worktree);
    });
    assert!(call(&state, cx, "caller", ordinary_worktree).is_err());

    set(cx, SettingsPatch::OrchestrateChildWorktrees(false));
    for (n, (mode, permitted)) in [
        (ChildApprovalMode::Orchestrator, "ask"),
        (ChildApprovalMode::Auto, "auto_review"),
        (ChildApprovalMode::AlwaysAllow, "full_access"),
        (ChildApprovalMode::Manual, "ask"),
    ]
    .into_iter()
    .enumerate()
    {
        set(cx, SettingsPatch::OrchestrateChildApproval(mode));
        let started = call(&state, cx, "caller", with(&format!("child-{n}"), &|_| {})).unwrap();
        assert_eq!(started["subagent"], true, "{started}");
        let id = started["thread_id"].as_str().unwrap().to_owned();
        let (_, message) = providers.started(cx, n).next_turn(cx);
        assert!(message.starts_with(MESSAGE));
        assert!(message.ends_with(crate::app::orchestrate::CHILD_REPORT_FOOTER));
        let meta = cx.seen().session(&id).unwrap();
        assert_eq!(meta.parent_session_id.as_deref(), Some("caller"));
        assert_eq!(meta.cwd, cwd, "the caller's own directory");
        assert_eq!(meta.report(), AgentReport::Expected);
        assert_eq!(
            permission(meta),
            Some(&serde_json::json!(permitted)),
            "{mode:?}"
        );
    }

    // The setting isolates a subagent by default; the project root is not a Git repository,
    // so no subagent starts, in the shared checkout or anywhere else.
    let before = started(cx).len();
    set(cx, SettingsPatch::OrchestrateChildWorktrees(true));
    let isolated = with("isolated", &|p| p.cwd = Some(".".into()));
    assert!(call(&state, cx, "caller", isolated).is_err());
    cx.run_until_parked();
    assert_eq!(started(cx).len(), before);
}

fn acp_test_settings() -> Settings {
    let mut settings = Settings::default();
    let agent = |id: &str, name: &str, enabled: bool| InstalledAgent {
        id: id.into(),
        name: name.into(),
        version: String::new(),
        icon: None,
        launch: agent::AcpLaunch::Custom {
            command: id.into(),
            args: Vec::new(),
            env: Vec::new(),
        },
        enabled,
        env: Vec::new(),
        launch_args: None,
    };
    settings.acp_agents.insert(
        "custom:antigravity".into(),
        agent("custom:antigravity", "Antigravity", true),
    );
    settings.acp_agents.insert(
        "custom:disabled".into(),
        agent("custom:disabled", "Disabled", false),
    );
    settings
        .acp_agents
        .insert("cursor".into(), agent("cursor", "Cursor", true));
    settings
}

/// `list_providers` omits ACP native duplicates that `start_thread` refuses, and
/// `start_thread` refuses invalid `acp_agent`, `profile`, `model` and approval
/// options with the expected reason.
#[test]
fn start_thread_validates_acp_agent_and_list_providers_filters_duplicates() {
    let store = TestStore::new("tcode-control-validate-acp");
    let root = store.root().join("project");
    stored_project(&store, "a", &root, |_| {});
    stored(
        &store,
        "caller",
        ProviderKind::ClaudeCode,
        "a",
        &root,
        |_| {},
    );
    tcode_services::settings::SettingsStore::new(store.root().clone())
        .save(&acp_test_settings())
        .unwrap();

    let cx = &mut TestAppContext::default();
    let (state, _providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    state
        .command(
            cx,
            Command::PatchSettings {
                patch: SettingsPatch::OrchestrateChildWorktrees(false),
            },
        )
        .unwrap();

    let listed = call(
        &state,
        cx,
        "caller",
        ControlOp::ListProviders(control_mcp::ListProvidersParams {}),
    )
    .unwrap();
    let acp_row = listed["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["kind"] == "acp")
        .expect("acp provider row");
    let listed_ids: Vec<&str> = acp_row["acp_agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["id"].as_str())
        .collect();
    assert!(listed_ids.contains(&"custom:antigravity"), "{listed_ids:?}");
    assert!(
        !listed_ids.contains(&"cursor"),
        "native duplicate must not appear in list_providers' acp_agents: {listed_ids:?}"
    );

    let with = |request_id: &str, edit: &dyn Fn(&mut StartThreadParams)| {
        let ControlOp::StartThread(mut params) = start("a") else {
            unreachable!()
        };
        params.request_id = request_id.into();
        params.subagent = Some(true);
        edit(&mut params);
        ControlOp::StartThread(params)
    };

    for (label, op, expected_reason) in [
        (
            "missing acp_agent",
            with("missing", &|p| p.provider = "acp".into()),
            "acp_agent is required",
        ),
        (
            "unknown acp_agent",
            with("unknown", &|p| {
                p.provider = "acp".into();
                p.acp_agent = Some("custom:nope".into());
            }),
            "unknown acp_agent: custom:nope",
        ),
        (
            "disabled acp_agent",
            with("disabled", &|p| {
                p.provider = "acp".into();
                p.acp_agent = Some("custom:disabled".into());
            }),
            "acp_agent custom:disabled is disabled",
        ),
        (
            "native-duplicate acp_agent",
            with("duplicate", &|p| {
                p.provider = "acp".into();
                p.acp_agent = Some("cursor".into());
            }),
            "duplicates a native provider",
        ),
        (
            "acp_agent on native provider",
            with("native-with-acp", &|p| {
                p.acp_agent = Some("custom:antigravity".into());
            }),
            "acp_agent is only valid when provider is acp",
        ),
        (
            "profile on acp",
            with("acp-profile", &|p| {
                p.provider = "acp".into();
                p.acp_agent = Some("custom:antigravity".into());
                p.profile = Some("custom-profile".into());
            }),
            "profile is not valid when provider is acp",
        ),
        (
            "model on acp",
            with("acp-model", &|p| {
                p.provider = "acp".into();
                p.acp_agent = Some("custom:antigravity".into());
                p.model = Some("gemini-4-neon".into());
            }),
            "acp:model",
        ),
        (
            "approval-governing option on acp",
            with("acp-mode", &|p| {
                p.provider = "acp".into();
                p.acp_agent = Some("custom:antigravity".into());
                p.options = Some(
                    [(
                        agent::ACP_MODE_OPTION_ID.to_owned(),
                        serde_json::json!("auto"),
                    )]
                    .into_iter()
                    .collect(),
                );
            }),
            "sets the permission mode",
        ),
    ] {
        let error = call(&state, cx, "caller", op).expect_err(&format!("{label} must be refused"));
        assert!(
            error.contains(expected_reason),
            "{label}: expected error containing `{expected_reason}`, got `{error}`"
        );
    }
    cx.run_until_parked();
    assert!(started(cx).is_empty());
}

/// An ACP subagent whose agent does not attach Tcode's MCP servers gets no
/// `report_result` reminder turn; completing its turn delivers its final
/// message to the parent as a labelled fallback immediately.
#[test]
fn acp_subagent_without_attached_mcp_falls_back_without_a_reminder() {
    let store = TestStore::new("tcode-control-start-acp-fallback");
    let root = store.root().join("project");
    stored_project(&store, "a", &root, |_| {});
    stored(
        &store,
        "caller",
        ProviderKind::ClaudeCode,
        "a",
        &root,
        |_| {},
    );
    tcode_services::settings::SettingsStore::new(store.root().clone())
        .save(&acp_test_settings())
        .unwrap();

    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    state
        .command(
            cx,
            Command::PatchSettings {
                patch: SettingsPatch::OrchestrateChildWorktrees(false),
            },
        )
        .unwrap();

    let ControlOp::StartThread(mut params) = start("a") else {
        unreachable!()
    };
    params.request_id = "acp-subagent".into();
    params.subagent = Some(true);
    params.provider = "acp".into();
    params.acp_agent = Some("custom:antigravity".into());
    params.options = Some(
        [("acp:model".to_owned(), serde_json::json!("gemini-4-neon"))]
            .into_iter()
            .collect(),
    );

    let reply = call(&state, cx, "caller", ControlOp::StartThread(params)).unwrap();
    assert_eq!(reply["subagent"], true, "{reply}");
    let id = reply["thread_id"].as_str().unwrap().to_owned();
    state.subscribe(
        cx,
        Topic::SessionStatus {
            session_id: id.clone(),
        },
    );
    let meta = cx.seen().session(&id).unwrap().clone();
    assert_eq!(meta.provider, ProviderKind::Acp);
    assert_eq!(meta.acp_agent_id.as_deref(), Some("custom:antigravity"));
    assert_eq!(meta.model, None);
    assert!(
        meta.option_selections
            .iter()
            .any(|s| s.id == "acp:model" && s.value == serde_json::json!("gemini-4-neon"))
    );
    assert_eq!(meta.parent_session_id.as_deref(), Some("caller"));

    let child = providers.started(cx, 0);
    let (delivery_id, text) = child.next_turn(cx);
    assert!(text.starts_with(MESSAGE));
    child.play([
        AgentEvent::McpServersNotAttached,
        AgentEvent::TurnAccepted { delivery_id },
        AgentEvent::ItemCompleted(agent::ThreadItem {
            id: "a1".into(),
            parent_item_id: None,
            content: agent::ItemContent::AssistantMessage {
                text: "ACP investigation complete".into(),
                summarized: false,
            },
        }),
        turn_completed(),
    ]);

    // The parent receives the labelled fallback right away, and the child
    // receives no reminder turn.
    let parent = providers.started(cx, 1);
    let (_, callback) = parent.next_turn(cx);
    assert!(
        callback.contains(tcode_core::notice::ORCHESTRATE_FALLBACK_LABEL),
        "parent must receive a labelled fallback: {callback}"
    );
    assert!(
        callback.contains("ACP investigation complete"),
        "fallback must carry the child's final message: {callback}"
    );
    cx.run_until_parked();
    assert!(
        next_command(&child.commands).is_err(),
        "child must receive no reminder turn when MCP servers were not attached"
    );
    assert!(cx.seen().status(&id).queued_messages.is_empty());
    assert_eq!(
        cx.seen().session(&id).unwrap().report(),
        tcode_core::settlement::AgentReport::Unreported
    );
}

/// An untitled `start_thread` thread is titled from its first message once the provider
/// accepts it, as a thread the user started is, though it runs in the background.
#[test]
fn an_untitled_started_thread_is_titled_from_its_first_message() {
    let store = TestStore::new("tcode-control-start-title");
    let root = store.root().join("project");
    stored_project(&store, "a", &root, |_| {});
    stored(
        &store,
        "caller",
        ProviderKind::ClaudeCode,
        "a",
        &root,
        |_| {},
    );
    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    let mut untitled = start("a");
    if let ControlOp::StartThread(params) = &mut untitled {
        params.title = None;
    }
    let id = call(&state, cx, "caller", untitled).unwrap()["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let provider = providers.started(cx, 0);
    let (delivery_id, _) = provider.next_turn(cx);
    provider.play([AgentEvent::TurnAccepted { delivery_id }]);
    cx.run_until_seen(|view| {
        view.session(&id)
            .is_some_and(|meta| !meta.title.starts_with("New "))
    });
    assert_eq!(cx.seen().session(&id).unwrap().title, MESSAGE);
}

/// `send_message` reaches threads in each mode as the agent's message, reopens a settled
/// thread and says so, and never counts as the user's activity.
#[test]
fn send_message_queues_steers_and_schedules_as_the_agent() {
    let store = TestStore::new("tcode-control-send");
    let root = store.root().clone();
    stored(
        &store,
        "caller",
        ProviderKind::ClaudeCode,
        "p",
        &root.join("caller"),
        |_| {},
    );
    for id in ["settled", "running", "later"] {
        stored(
            &store,
            id,
            ProviderKind::Codex,
            "p",
            &root.join(id),
            |meta| {
                if id == "settled" {
                    meta.settled_at = Some(1);
                }
            },
        );
    }
    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    for id in ["settled", "later"] {
        state.open(cx, id);
    }
    let running = first_turn(&state, cx, &providers, "running", 0);
    // A provider started from here on is still starting, so what it is to be sent waits
    // where a client sees it.
    providers.hold();
    let text = "Rebase on main when you can";
    let send = |ids: &[&str], mode: SendMode| {
        ControlOp::SendMessage(SendMessageParams {
            thread_ids: ids.iter().map(|id| id.to_string()).collect(),
            text: text.into(),
            mode,
            attachments: None,
            request_id: None,
        })
    };
    let last_human = |cx: &mut TestAppContext| {
        ["settled", "running", "later"].map(|id| {
            let shown = ControlOp::GetThread(GetThreadParams {
                thread_id: id.into(),
            });
            call(&state, cx, "caller", shown).unwrap()["last_human_message_at"].clone()
        })
    };
    let before = last_human(cx);
    let at = cx.now_secs() + 3600;
    let hosts = ControlOp::SendMessage(SendMessageParams {
        attachments: Some(vec![std::env::temp_dir().display().to_string()]),
        ..match send(&["later"], SendMode::Queue) {
            ControlOp::SendMessage(params) => params,
            _ => unreachable!(),
        }
    });
    assert!(call(&state, cx, "caller", hosts).is_err());

    let queued = call(&state, cx, "caller", send(&["settled"], SendMode::Queue)).unwrap();
    let steer = call(
        &state,
        cx,
        "caller",
        send(&["running", "settled"], SendMode::Steer),
    )
    .unwrap();
    let scheduled = call(
        &state,
        cx,
        "caller",
        send(&["later"], SendMode::Schedule { at }),
    )
    .unwrap();

    assert_eq!(queued["results"][0]["outcome"], "applied");
    assert_eq!(queued["results"][0]["reason"], "queued");
    assert!(
        queued["results"][0]["side_effects"].is_array(),
        "reopening a settled thread is named as a side effect"
    );
    assert_eq!(steer["results"][0]["reason"], "steered");
    assert_eq!(steer["results"][1]["reason"], "queued");
    assert!(
        steer["results"][1]["side_effects"].is_array(),
        "a steer that was queued instead says so"
    );
    assert_eq!(scheduled["results"][0]["reason"], "scheduled");

    assert_eq!(running.next_steer(cx), text);
    running.fence(cx, "running");
    assert_eq!(said(cx, "running", MessageOrigin::Agent), [text]);
    assert!(!cx.seen().session("settled").unwrap().is_settled());
    let waiting = |cx: &TestAppContext, id: &str| cx.seen().status(id).queued_messages.clone();
    assert_eq!(
        waiting(cx, "settled")
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        [text, text]
    );
    assert_eq!(waiting(cx, "later")[0].fire_at_unix_secs, Some(at));
    assert_eq!(
        last_human(cx),
        before,
        "an agent's message is not the user's activity"
    );

    providers.let_go();
    let settled = providers.started_in(cx, &root.join("settled"));
    settled.start_turn(cx, "turn");
    settled.fence(cx, "settled");
    assert_eq!(said(cx, "settled", MessageOrigin::Agent)[0], text);
}

/// A queued message is dropped and the others stay; the permission mode stays the user's;
/// a fork answers with the new thread's id.
#[test]
fn control_thread_drops_a_queued_message_keeps_the_permission_mode_and_forks() {
    let store = TestStore::new("tcode-control-thread");
    let root = store.root().clone();
    let resumable = |meta: &mut SessionMeta| {
        meta.resume_cursor = Some(agent::ResumeCursor(serde_json::json!({"thread_id": "t"})))
    };
    stored(
        &store,
        "caller",
        ProviderKind::ClaudeCode,
        "p",
        &root.join("caller"),
        |_| {},
    );
    stored(
        &store,
        "queued",
        ProviderKind::Codex,
        "p",
        &root.join("queued"),
        |_| {},
    );
    for id in ["source", "history"] {
        stored(
            &store,
            id,
            ProviderKind::Codex,
            "p",
            &root.join(id),
            resumable,
        );
    }
    // Two turns of `history`, the second with a checkpoint to fork before.
    for (ts, event) in [
        AgentEvent::ItemCompleted(agent::ThreadItem {
            id: "first".into(),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: "first".into(),
                attachments: Vec::new(),
            },
        }),
        AgentEvent::TurnCompleted {
            turn_id: "t1".into(),
            status: agent::TurnStatus::Completed,
            usage: None,
        },
        AgentEvent::ItemCompleted(agent::ThreadItem {
            id: "second".into(),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: "second".into(),
                attachments: Vec::new(),
            },
        }),
        AgentEvent::TurnCheckpoint {
            turn_id: String::new(),
            checkpoint_id: "t2".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        store
            .append_event("history", 1_000 + ts as u64, &event)
            .unwrap();
    }
    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    state.open(cx, "history");
    // A turn runs in `queued`, so what is sent to it now waits its turn.
    first_turn(&state, cx, &providers, "queued", 0);
    state
        .command(
            cx,
            Command::ScheduleTurn {
                session_id: "queued".into(),
                text: "from the user".into(),
                attachment_paths: Vec::new(),
                due: TurnDue::AfterSecs(3600),
            },
        )
        .unwrap();
    let sent = ControlOp::SendMessage(SendMessageParams {
        thread_ids: vec!["queued".into()],
        text: "from an agent".into(),
        mode: SendMode::Queue,
        attachments: None,
        request_id: None,
    });
    call(&state, cx, "caller", sent).unwrap();
    cx.run_until_parked();
    let waiting = |cx: &TestAppContext| {
        cx.seen()
            .status("queued")
            .queued_messages
            .iter()
            .map(|message| (message.id, message.text.clone()))
            .collect::<Vec<_>>()
    };
    let agent = waiting(cx)
        .into_iter()
        .find(|(_, text)| text == "from an agent")
        .expect("the agent's message waits behind the running turn")
        .0;

    let change = |thread_id: &str, op: RunOp| {
        ControlOp::ControlThread(ControlThreadParams {
            changes: vec![RunChange {
                thread_id: thread_id.into(),
                op,
            }],
            request_id: None,
        })
    };
    let dropped = call(
        &state,
        cx,
        "caller",
        change("queued", RunOp::DropQueued { id: agent }),
    )
    .unwrap();
    assert_eq!(dropped["results"][0]["outcome"], "applied");
    let permitted = call(
        &state,
        cx,
        "caller",
        change(
            "queued",
            RunOp::SetOption {
                id: "permissions".into(),
                value: serde_json::json!("full_access"),
            },
        ),
    )
    .unwrap();
    assert_eq!(permitted["results"][0]["outcome"], "failed");
    cx.run_until_parked();
    assert_ne!(
        permission(cx.seen().session("queued").unwrap()),
        Some(&serde_json::json!("full_access")),
        "the permission mode stays the user's"
    );
    assert_eq!(
        waiting(cx)
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        ["from the user"]
    );

    let forked = call(&state, cx, "caller", change("source", RunOp::Fork)).unwrap();
    assert_eq!(forked["results"][0]["outcome"], "applied");
    let fork = forked["results"][0]["new_thread_id"].as_str().unwrap();
    assert_ne!(fork, "source");
    cx.run_until_parked();
    assert!(cx.seen().session(fork).is_some());

    let refused = call(
        &state,
        cx,
        "caller",
        change("history", RunOp::ForkAt { turn: 3 }),
    )
    .unwrap();
    assert_eq!(refused["results"][0]["outcome"], "failed");
    let forked = call(
        &state,
        cx,
        "caller",
        change("history", RunOp::ForkAt { turn: 0 }),
    )
    .unwrap();
    assert_eq!(forked["results"][0]["outcome"], "applied");
    let fork = forked["results"][0]["new_thread_id"].as_str().unwrap();
    cx.run_until_parked();
    let fork = cx.seen().session(fork).unwrap();
    assert_eq!(
        fork.fork_origin
            .as_ref()
            .map(|origin| origin.thread_id.as_str()),
        Some("history")
    );
    assert_eq!(
        fork.pending_fork,
        Some(agent::ForkPoint::Before {
            checkpoint_id: "t2".into()
        })
    );
}

/// `git` answers with what git did: checkout refuses a dirty tree and a thread whose turn is
/// running, commit reports git's output, and a clean tree then checks out.
#[test]
fn git_answers_with_the_outcome_and_refuses_unsafe_checkouts() {
    let store = TestStore::new("tcode-control-git");
    let root = store.root().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    for args in [
        &["init", "-b", "main"][..],
        &["config", "commit.gpgsign", "false"],
        &["config", "core.hooksPath", ".no-hooks"],
        &["config", "user.name", "tcode"],
        &["config", "user.email", "tcode@localhost"],
    ] {
        run_git(&root, args).unwrap();
    }
    std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
    run_git(&root, &["add", "tracked.txt"]).unwrap();
    run_git(&root, &["commit", "-m", "initial"]).unwrap();
    run_git(&root, &["branch", "topic"]).unwrap();
    std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();

    stored(
        &store,
        "caller",
        ProviderKind::ClaudeCode,
        "p",
        &store.root().join("caller"),
        |_| {},
    );
    stored(&store, "target", ProviderKind::Codex, "p", &root, |_| {});
    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &store);
    let git = |cx: &mut TestAppContext, op: GitOp| {
        let op = ControlOp::Git(GitParams {
            thread_id: "target".into(),
            op,
        });
        call(&state, cx, "caller", op).unwrap()
    };
    let checkout = || GitOp::CheckoutBranch {
        branch: "topic".into(),
    };

    let dirty = git(cx, checkout());
    assert_eq!(dirty["outcome"], "failed");

    let committed = git(
        cx,
        GitOp::Commit {
            message: "Record the change".into(),
            included: None,
        },
    );
    assert_eq!(committed["outcome"], "applied");
    assert!(
        committed["output"]
            .as_str()
            .unwrap()
            .contains("Record the change")
    );

    let target = first_turn(&state, cx, &providers, "target", 0);
    let busy = git(cx, checkout());
    assert_eq!(busy["outcome"], "skipped");
    target.play([turn_completed()]);
    cx.run_until_seen(|view| view.status("target").running_turn.is_none());

    assert_eq!(git(cx, checkout())["outcome"], "applied");
    assert_eq!(
        run_git(&root, &["branch", "--show-current"])
            .unwrap()
            .trim(),
        "topic"
    );
}

/// A quick chat moves itself into another project. The move is audited in the thread with
/// the project it went to, and the turn that asked keeps its provider until it ends, when
/// the provider leaves the old directory.
#[test]
fn move_thread_moves_the_caller_and_keeps_its_turn() {
    let store = TestStore::new("tcode-control-move-thread");
    let quick = store.root().join("quick");
    let demo = store.root().join("demo");
    stored_project(&store, "quick", &quick, |_| {});
    stored_project(&store, "demo", &demo, |_| {});
    stored(
        &store,
        "caller",
        ProviderKind::ClaudeCode,
        "quick",
        &quick,
        |_| {},
    );
    let cx = &mut TestAppContext::default();
    let (state, providers) = played_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    let caller = first_turn(&state, cx, &providers, "caller", 0);

    let moved = call(
        &state,
        cx,
        "caller",
        ControlOp::MoveThread(MoveThreadParams {
            thread_id: "caller".into(),
            project_id: "demo".into(),
            cwd: None,
            request_id: Some("move-2".into()),
        }),
    )
    .unwrap();
    assert_eq!(moved["project_id"], "demo");
    assert_eq!(moved["cwd"], demo.display().to_string());
    cx.run_until_parked();
    let shown = cx.seen().session("caller").unwrap();
    assert_eq!(shown.project_id.as_deref(), Some("demo"));
    assert_eq!(shown.cwd, demo);
    assert!(
        next_command(&caller.commands).is_err(),
        "the turn that asked keeps its provider"
    );
    let audits: Vec<_> = cx
        .seen()
        .timeline("caller")
        .entries
        .iter()
        .filter_map(|entry| match &entry.content {
            EntryContent::ControlAction(action) => {
                Some((action.action.clone(), action.detail.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        audits,
        [(
            tcode_core::control::action::MOVE.to_owned(),
            Some(format!("demo ({})", demo.display()))
        )]
    );

    caller.play([turn_completed()]);
    assert!(
        matches!(caller.next_command(cx), SessionCommand::Shutdown),
        "its turn over, the provider leaves the old directory"
    );
}

/// A thread put under a collaboration runs on its decision model, whose provider is told the
/// collaboration at launch: each collaborator's mandate, and what is known of how its model
/// is best used. The thread's status discloses that text as delivered. As subagents it
/// starts the collaborators and no other model, each at a reasoning effort the collaboration
/// allows it, and `tcode_control` starts and lists nothing that Settings → Control leaves
/// out.
#[test]
fn a_thread_under_a_collaboration_is_told_it_and_starts_its_collaborators_only() {
    use tcode_core::collaboration::{Collaboration, CollaborationMember, ModelGuide, ModelRef};
    use tcode_core::control::{AgentGrant, AgentScope, ModelGrant};
    use tcode_protocol::InjectedChannel;

    let store = TestStore::new("tcode-control-collaboration");
    let root = store.root().join("project");
    stored_project(&store, "a", &root, |_| {});
    stored(&store, "lead", ProviderKind::Codex, "a", &root, |_| {});
    let cx = &mut TestAppContext::default();
    let (state, providers) = controlled_host(cx, &store);
    state.subscribe(cx, Topic::Index);
    let set = |cx: &mut TestAppContext, patch: SettingsPatch| {
        state.command(cx, Command::PatchSettings { patch }).unwrap();
    };
    let model = |provider, model: &str| ModelRef {
        provider,
        profile_id: None,
        model: model.into(),
    };
    set(
        cx,
        SettingsPatch::SaveCollaboration(Collaboration {
            id: "delivery".into(),
            name: "Delivery".into(),
            lead: model(ProviderKind::ClaudeCode, "decider"),
            lead_mandate: String::new(),
            members: vec![CollaborationMember {
                id: "m".into(),
                model: model(ProviderKind::Codex, "executor"),
                mandate: "Implements what it is handed.".into(),
                efforts: vec!["high".into()],
            }],
            favorite: false,
        }),
    );
    set(
        cx,
        SettingsPatch::ModelGuide(ModelGuide {
            model: model(ProviderKind::Codex, "executor"),
            text: "Brief it one change at a time.".into(),
            thread_id: None,
            researched_at: None,
        }),
    );

    state.open(cx, "lead");
    state
        .command(
            cx,
            Command::SetActiveCollaboration {
                session_id: "lead".into(),
                collaboration_id: "delivery".into(),
            },
        )
        .unwrap();
    let lead = first_turn(&state, cx, &providers, "lead", 0);
    assert_eq!(lead.provider, ProviderKind::ClaudeCode);
    assert_eq!(lead.options.model.as_deref(), Some("decider"));
    let brief = lead
        .options
        .instructions
        .clone()
        .expect("the decision model is told the collaboration");
    assert!(brief.contains("Implements what it is handed."), "{brief}");
    assert!(brief.contains("Brief it one change at a time."), "{brief}");
    lead.fence(cx, "lead");
    let disclosed = cx.seen().status("lead").injected_tools.clone().unwrap();
    assert!(
        disclosed.instructions.iter().any(|told| {
            told.channel == InjectedChannel::Collaboration(agent::InstructionChannel::SessionAppend)
                && told.text == brief
        }),
        "{:?}",
        disclosed.instructions
    );

    let with = |request_id: &str, subagent: bool, model: &str| {
        let ControlOp::StartThread(mut params) = start("a") else {
            unreachable!()
        };
        params.request_id = request_id.into();
        params.subagent = subagent.then_some(true);
        params.model = Some(model.into());
        ControlOp::StartThread(params)
    };
    let collaborator = call(&state, cx, "lead", with("collaborator", true, "executor")).unwrap();
    assert_eq!(collaborator["subagent"], true, "{collaborator}");
    // It runs at the one effort the collaboration allows it, and at no other.
    let effort = |meta: &SessionMeta| {
        meta.option_selections
            .iter()
            .find(|selection| selection.id == agent::EFFORT_OPTION_ID)
            .map(|selection| selection.value.clone())
    };
    let started = cx
        .seen()
        .session(collaborator["thread_id"].as_str().unwrap())
        .unwrap()
        .clone();
    assert_eq!(effort(&started), Some(serde_json::json!("high")));
    let ControlOp::StartThread(mut lighter) = with("lighter", true, "executor") else {
        unreachable!()
    };
    lighter.options = Some(
        [(agent::EFFORT_OPTION_ID.to_owned(), serde_json::json!("low"))]
            .into_iter()
            .collect(),
    );
    assert!(call(&state, cx, "lead", ControlOp::StartThread(lighter)).is_err());
    // Another model is refused as a subagent only: the collaboration is what refuses it.
    assert!(call(&state, cx, "lead", with("other-subagent", true, "another")).is_err());
    assert!(call(&state, cx, "lead", with("other-thread", false, "another")).is_ok());

    set(
        cx,
        SettingsPatch::ControlAgents(AgentScope::Only(vec![AgentGrant {
            profile_id: "codex".into(),
            models: ModelGrant::Only(vec!["executor".into()]),
            acp_agents: Vec::new(),
        }])),
    );
    assert!(call(&state, cx, "lead", with("withheld", false, "another")).is_err());
    assert!(call(&state, cx, "lead", with("let", false, "executor")).is_ok());
    let listed = call(
        &state,
        cx,
        "lead",
        ControlOp::ListProviders(Default::default()),
    )
    .unwrap();
    let listed: Vec<(&str, &serde_json::Value)> = listed["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["kind"].as_str().unwrap(),
                &row["profiles"][0]["startable_models"],
            )
        })
        .collect();
    assert_eq!(listed, [("codex", &serde_json::json!(["executor"]))]);
}

/// The thread the user opened to research a model's guide is the one thread offered
/// `set_model_guide`, and its call writes that model's guide as that thread's. Another
/// thread is neither offered the tool nor writes a guide by calling it.
#[test]
fn only_the_thread_researching_a_guide_is_given_the_tool_that_writes_it() {
    use control_mcp::SetModelGuideParams;
    use tcode_core::collaboration::ModelRef;

    let store = TestStore::new("tcode-control-model-guide");
    let root = store.root().join("project");
    stored_project(&store, "a", &root, |_| {});
    let researched = ModelRef {
        provider: ProviderKind::Codex,
        profile_id: None,
        model: "executor".into(),
    };
    stored(
        &store,
        "research",
        ProviderKind::Codex,
        "a",
        &root,
        |meta| meta.guide_for = Some(researched.clone()),
    );
    stored(&store, "other", ProviderKind::Codex, "a", &root, |_| {});
    let cx = &mut TestAppContext::default();
    let (state, _providers) = controlled_host(cx, &store);
    state.open(cx, "research");
    state.open(cx, "other");
    let offered = |cx: &TestAppContext, id: &str| {
        let disclosed = cx.seen().status(id).injected_tools.clone().unwrap();
        disclosed
            .tools
            .iter()
            .any(|(name, _)| name == "set_model_guide")
    };
    assert!(offered(cx, "research"));
    assert!(!offered(cx, "other"));

    let write = || {
        ControlOp::SetModelGuide(SetModelGuideParams {
            guide: "Brief it one change at a time.".into(),
        })
    };
    assert!(call(&state, cx, "other", write()).is_err());
    call(&state, cx, "research", write()).unwrap();
    let guides = state.read(|state| state.settings.model_guides.clone());
    assert_eq!(guides.len(), 1, "{guides:?}");
    assert_eq!(guides[0].model, researched);
    assert_eq!(guides[0].text, "Brief it one change at a time.");
    assert_eq!(guides[0].thread_id.as_deref(), Some("research"));
}
