use super::*;
use crate::app::test_support::*;
use serde_json::{Value, json};
use std::sync::Mutex;
use tcode_protocol::Command;
use tcode_services::settings::SettingsStore;

fn linked_meta(id: &str, settled: bool) -> SessionMeta {
    let mut meta = SessionMeta::new(
        ProviderKind::Codex,
        PathBuf::from("/tmp/synthetic-checkout"),
        None,
    );
    meta.id = id.into();
    meta.project_id = Some("sample".into());
    meta.settled_at = settled.then_some(1);
    pull_request::link_pull_request(
        &mut meta.pull_requests,
        PullRequestKey::new("github.com", "sample/project", 1),
        "https://github.com/sample/project/pull/1".into(),
        PullRequestSource::Manual,
        1,
        true,
    );
    meta
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = tcode_services::process::command("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn mcp_child_linking_is_bound_to_its_token_and_rejects_a_thread_override() {
    let store = TestStore::new("tcode-pr-mcp");
    let cwd = store.root().join("checkout");
    std::fs::create_dir(&cwd).unwrap();
    git(&cwd, &["init", "-b", "main"]);
    git(
        &cwd,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/sample/project.git",
        ],
    );
    let settings_store = SettingsStore::new(store.root().clone());
    let mut settings = settings_store.try_load().unwrap();
    settings.source_control.hosts.insert(
        "github.com".into(),
        tcode_core::settings::HostSettings {
            enabled: false,
            ..tcode_core::settings::HostSettings::new(pull_request::HostKind::Github)
        },
    );
    settings_store.save(&settings).unwrap();
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let mut host = mcp_host::Host::bind().unwrap();
    let server = pull_request_mcp::start(&mut host);
    let registration = state.update(&mut cx, |state, cx| {
        state.pump_pull_request_requests(Some(server), cx);
        let mut parent = linked_meta("parent", false);
        parent.cwd = cwd;
        parent.pull_requests.clear();
        let mut child = parent.clone();
        child.id = "child".into();
        child.parent_session_id = Some(parent.id.clone());
        state.sessions.extend([parent, child.clone()]);
        state.pull_request_registration_for(&child).unwrap()
    });
    host.start().unwrap();
    let result = Arc::new(Mutex::new(None));
    let completed = result.clone();
    let job = std::thread::spawn(move || {
        let (_, session) = rpc(
            &registration.url,
            &registration.bearer_token,
            None,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"contract-fixture","version":"1"}}}),
        );
        rpc(
            &registration.url,
            &registration.bearer_token,
            session.as_deref(),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        );
        let call = |id, name, args| {
            rpc(
                &registration.url,
                &registration.bearer_token,
                session.as_deref(),
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "tools/call",
                    "params": {"name": name, "arguments": args},
                }),
            )
            .0
        };
        let linked = call(
            2,
            "link_pull_request",
            json!({"url":"https://github.com/sample/project/pull/1?view=1#issuecomment-7"}),
        );
        let forbidden = call(
            3,
            "link_pull_request",
            json!({"url":"https://github.com/sample/project/pull/2","threadId":"parent"}),
        );
        let listed = call(4, "list_thread_pull_requests", json!({}));
        let unlinked = call(
            5,
            "unlink_pull_request",
            json!({"repository":"sample/project","number":1}),
        );
        *completed.lock().unwrap() = Some((linked, forbidden, listed, unlinked));
    });
    cx.run_until(|_| result.lock().unwrap().is_some());
    job.join().unwrap();
    let (linked, forbidden, listed, unlinked) = result.lock().unwrap().take().unwrap();
    assert_eq!(
        linked["result"]["isError"], false,
        "link response: {linked}"
    );
    assert!(forbidden["error"].is_object() || forbidden["result"]["isError"] == true);
    let listed: Value =
        serde_json::from_str(listed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(listed["pullRequests"][0]["number"], 1);
    assert_eq!(listed["pullRequests"][0]["source"], "agent");
    assert_eq!(
        unlinked["result"]["isError"], false,
        "unlink response: {unlinked}"
    );
    assert!(state.read(|state| {
        state.find_meta("parent").unwrap().pull_requests.is_empty()
            && state
                .find_meta("child")
                .unwrap()
                .pull_requests
                .iter()
                .all(|link| !link.visible())
    }));
}

/// The linking instructions reach the provider at launch, for its own instruction channel; no
/// turn carries them, so the stored user turn is the user's text alone, and what the agent sees
/// holds the delivered text verbatim under that channel. A provider that took none of Tcode's MCP
/// servers is shown the tools as offered, with no instructions.
#[test]
fn instructions_reach_the_provider_at_launch_and_no_turn_carries_them() {
    use tcode_protocol::{InjectedChannel, InjectedInstructions};
    let store = TestStore::new("tcode-pr-instructions");
    let mut host = mcp_host::Host::bind().unwrap();
    let server = pull_request_mcp::start(&mut host);
    let scripted = scripted_provider(ProviderKind::ClaudeCode);
    let (launched_tx, launched) = smol::channel::unbounded();
    let scripted_launcher = scripted.launcher.clone();
    let launcher = ProviderLauncher(Arc::new(move |kind, options: agent::SessionOptions| {
        launched_tx.try_send(options.clone()).unwrap();
        scripted_launcher.launch(kind, options)
    }));
    let mut cx = TestAppContext::default();
    let state = cx.new_entity({
        let mut state = TestClientState::new((*store).clone());
        state.set_provider_launcher(launcher);
        state
    });
    state.update(&mut cx, |state, cx| {
        state.pump_pull_request_requests(Some(server), cx);
        let mut meta = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/synthetic-checkout"),
            None,
        );
        meta.id = "thread".into();
        store.upsert_meta(&meta).unwrap();
        state.sessions.push(meta);
        state.select_session("thread", cx);
    });
    state.dispatch_command(
        &mut cx,
        1,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "fix the build".into(),
            attachment_paths: Vec::new(),
        },
    );
    let commands = scripted.commands.clone();
    let SessionCommand::SendTurn {
        text, delivery_id, ..
    } = cx.run_until_recv(&commands)
    else {
        panic!("expected a provider delivery")
    };
    assert_eq!(text, "fix the build");
    let options = launched.try_recv().unwrap();
    let linking = pull_request::linking_instructions(&pull_request::hosts_in([]));
    assert_eq!(options.instructions.as_deref(), Some(linking.as_str()));
    assert!(
        options
            .mcp_servers
            .iter()
            .any(|server| server.name == "tcode_pull_requests")
    );

    scripted
        .events
        .try_send(AgentEvent::TurnAccepted { delivery_id })
        .unwrap();
    cx.run_until(|state| {
        state
            .resident("thread")
            .is_some_and(|session| session.delivery_in_flight.is_none())
    });
    let stored = store
        .read_events("thread")
        .unwrap()
        .into_iter()
        .find_map(|event| match event.event {
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::UserMessage { text, .. },
                ..
            }) => Some(text),
            _ => None,
        });
    assert_eq!(stored.as_deref(), Some("fix the build"));

    let disclosed = |state: &TestEntity| {
        state.read(|state| {
            state
                .session_status_snapshot("thread")
                .unwrap()
                .injected_tools
                .unwrap()
        })
    };
    let tools = disclosed(&state);
    assert!(tools.attached);
    assert_eq!(
        tools.instructions,
        [
            InjectedInstructions {
                channel: InjectedChannel::Linking(agent::InstructionChannel::SessionAppend),
                text: linking,
            },
            InjectedInstructions {
                channel: InjectedChannel::McpServer("tcode_pull_requests".into()),
                text: pull_request_mcp::INSTRUCTIONS.into(),
            },
        ]
    );

    scripted
        .events
        .try_send(AgentEvent::McpServersNotAttached)
        .unwrap();
    cx.run_until(|state| {
        let tools = state.session_status_snapshot("thread").unwrap();
        !tools.injected_tools.unwrap().attached
    });
    let tools = disclosed(&state);
    assert!(tools.instructions.is_empty());
    assert!(!tools.tools.is_empty());
}

/// A host started the way every composition root starts one discloses the tools to each client,
/// for the providers that receive them.
#[test]
fn the_agent_tools_are_disclosed_before_the_first_turn() {
    let store = TestStore::new("tcode-pr-tools-disclosed");
    let mut services = crate::pipe::HostServices::default();
    services.start_mcp_servers(false);
    assert_eq!(services.mcp_unavailable, None);
    let host = crate::pipe::spawn_host((*store).clone(), services).unwrap();
    for (provider, disclosed) in [(ProviderKind::ClaudeCode, true), (ProviderKind::Pi, false)] {
        let tools = smol::block_on(host.update_state_for_test(move |state, _| {
            let meta = SessionMeta::new(provider, PathBuf::from("/tmp/synthetic-checkout"), None);
            let id = meta.id.clone();
            state.sessions.push(meta.clone());
            state
                .residents
                .live
                .insert(id.clone(), ActiveSession::new(meta, false, Vec::new()));
            state.session_status_snapshot(&id).unwrap().injected_tools
        }))
        .unwrap();
        assert_eq!(
            tools.is_some(),
            disclosed,
            "{provider:?}: shown exactly when its provider would receive the tools"
        );
        if let Some(tools) = tools {
            let description = preview_mcp::tool_descriptions().into_iter().next().unwrap();
            assert!(tools.tools.contains(&description));
        }
    }
    host.shutdown_blocking().unwrap();
}

/// The tools a thread was registered with keep the hosts they named, so what the agent sees
/// matches what it has when a host is added mid-thread; a later thread names the new host.
#[test]
fn a_threads_tools_keep_the_hosts_they_were_registered_with() {
    let store = TestStore::new("tcode-pr-tools-hosts");
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let mut host = mcp_host::Host::bind().unwrap();
    let server = pull_request_mcp::start(&mut host);
    state.update(&mut cx, |state, cx| {
        state.pump_pull_request_requests(Some(server), cx)
    });
    let listed = |state: &mut AppState, id: &str| {
        state
            .session_status_snapshot(id)
            .unwrap()
            .injected_tools
            .unwrap()
            .tools
            .into_iter()
            .find(|(name, _)| name == "list_thread_pull_requests")
            .unwrap()
            .1
    };
    state.update(&mut cx, |state, _| {
        let meta = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/synthetic-checkout"),
            None,
        );
        let before = meta.id.clone();
        state.pull_request_registration_for(&meta).unwrap();
        state.sessions.push(meta.clone());
        state.install_selected(ActiveSession::new(meta, false, Vec::new()));
        state.settings.source_control.hosts.insert(
            "codeberg.org".into(),
            tcode_core::settings::HostSettings::new(pull_request::HostKind::Forgejo),
        );
        state.name_pull_request_hosts();
        assert!(listed(state, &before).contains("without asking GitHub."));
        let meta = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/tmp/synthetic-checkout"),
            None,
        );
        let after = meta.id.clone();
        state.pull_request_registration_for(&meta).unwrap();
        state.sessions.push(meta.clone());
        state.install_selected(ActiveSession::new(meta, false, Vec::new()));
        let text = listed(state, &after);
        assert!(text.contains("without asking GitHub or Forgejo."), "{text}");
    });
}
