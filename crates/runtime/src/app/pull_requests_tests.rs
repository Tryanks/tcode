use super::*;
use crate::app::test_support::*;
use serde_json::{Value, json};
use std::sync::Mutex;
use tcode_core::pull_request::PullRequestStack;
use tcode_services::{github::Credentials, settings::SettingsStore};

#[path = "../../../services/tests/support/github.rs"]
#[allow(
    dead_code,
    reason = "The same HTTP fixture also supplies exchange helpers to services tests."
)]
mod fixture;

struct HostReply {
    state: &'static str,
    stack: bool,
    requests: Vec<Value>,
}
fn pr(number: u64, state: &str, stack: bool) -> Value {
    json!({"number":number,"url":format!("https://github.com/sample/project/pull/{number}"),"title":format!("Change {number}"),"state":state,"isDraft":false,"headRefName":format!("layer-{number}"),"baseRefName":"main","updatedAt":"2026-10-08T00:00:00Z","additions":3,"deletions":1,"changedFiles":1,"reviewDecision":"APPROVED","mergeable":"MERGEABLE","stack":if stack {json!({"number":7})}else{Value::Null}})
}
fn client(store: &SessionStore, fixture: &fixture::Fixture) -> Arc<GitHubApi> {
    GitHubApi::new(
        Credentials::new(
            SettingsStore::new(store.root().to_path_buf()),
            [("GH_TOKEN".into(), "fixture".into())],
        ),
        fixture.builder(),
    )
}
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
fn sweep(state: &TestEntity, cx: &mut TestAppContext) {
    state.update(cx, |state, cx| {
        state.sweep_pull_requests(false, cx).detach()
    });
    cx.run_until(|state| !state.pull_requests.syncing);
}

#[test]
fn shared_sync_changes_only_observations_and_honors_terminal_cadence() {
    let store = TestStore::new("tcode-pr-sync");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let response = Arc::new(Mutex::new(HostReply {
        state: "OPEN",
        stack: false,
        requests: Vec::new(),
    }));
    let responding = response.clone();
    let _server = fixture.serve(move |exchange| {
        let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
        let mut model = responding.lock().unwrap();
        model.requests.push(sent.clone());
        let mut data = serde_json::Map::new();
        for (name, number) in sent["variables"].as_object().unwrap() {
            if let Some(alias) = name.strip_suffix("_number") {
                data.insert(
                    alias.into(),
                    json!({"pullRequest":pr(number.as_u64().unwrap(),model.state,model.stack)}),
                );
            }
        }
        exchange.reply(200, "", &serde_json::to_vec(&json!({"data":data})).unwrap());
    });
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        for meta in [linked_meta("active", false), linked_meta("settled", true)] {
            store.upsert_meta(&meta).unwrap();
            state.sessions.push(meta);
        }
    });
    sweep(&state, &mut cx);
    assert_eq!(
        response.lock().unwrap().requests.len(),
        1,
        "a PR shared by two threads needs one read"
    );
    let first = store.load_index().unwrap();
    assert!(first.iter().all(|meta| {
        meta.pull_requests[0]
            .snapshot
            .as_ref()
            .is_some_and(|s| s.state == PullRequestState::Open)
    }));
    sweep(&state, &mut cx);
    assert_eq!(
        store.load_index().unwrap(),
        first,
        "unchanged state must preserve activity, unread state and synced_at"
    );
    response.lock().unwrap().state = "CLOSED";
    sweep(&state, &mut cx);
    let closed_reads = response.lock().unwrap().requests.len();
    sweep(&state, &mut cx);
    assert_eq!(response.lock().unwrap().requests.len(), closed_reads);
    state.update(&mut cx, |state, _| {
        state.pull_requests.last_synced.insert(
            PullRequestKey::new("github.com", "sample/project", 1),
            now_secs() - 900,
        );
    });
    sweep(&state, &mut cx);
    assert_eq!(response.lock().unwrap().requests.len(), closed_reads + 1);
    response.lock().unwrap().state = "MERGED";
    state.update(&mut cx, |state, cx| {
        state.refresh_thread_pull_requests("active", cx)
    });
    cx.run_until(|state| {
        !state.pull_requests.syncing
            && state.find_meta("active").unwrap().pull_requests[0]
                .snapshot
                .as_ref()
                .unwrap()
                .state
                == PullRequestState::Merged
    });
    let merged_reads = response.lock().unwrap().requests.len();
    sweep(&state, &mut cx);
    assert_eq!(response.lock().unwrap().requests.len(), merged_reads);
    state.update(&mut cx, |state, cx| {
        state.unlink_pull_request(
            "active",
            &PullRequestKey::new("github.com", "sample/project", 1),
            cx,
        )
    });
    sweep(&state, &mut cx);
    assert!(state.read(|state| state.find_meta("active").unwrap().pull_requests.is_empty()));
    assert_eq!(
        response.lock().unwrap().requests.len(),
        merged_reads,
        "settled-only links are not independently due"
    );
}

#[test]
fn native_stack_sync_preserves_dismissals_and_explicit_restore_has_no_watch() {
    let store = TestStore::new("tcode-pr-stack");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let _server = fixture.serve(|exchange| {
        if exchange.request.starts_with("GET ") {
            exchange.reply(200,"",br#"[{"id":"stack-7","number":7,"url":"https://github.com/sample/project/stack/7","base":{"ref":"main"},"pull_requests":[{"number":1,"head":{"ref":"layer-1"},"state":"merged"},{"number":2,"head":{"ref":"layer-2"},"state":"open"}]}]"#);
        } else {
            let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
            let mut data = serde_json::Map::new();
            for (name,number) in sent["variables"].as_object().unwrap() {
                if let Some(alias) = name.strip_suffix("_number") {
                    data.insert(alias.into(),json!({"pullRequest":pr(number.as_u64().unwrap(), if number == 1 {"MERGED"}else{"OPEN"},true)}));
                }
            }
            exchange.reply(200,"", &serde_json::to_vec(&json!({"data":data})).unwrap());
        }
    });
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        state
            .sessions
            .extend([linked_meta("active", false), linked_meta("settled", true)]);
    });
    sweep(&state, &mut cx);
    cx.run_until(|state| !state.pull_requests.syncing);
    assert_eq!(
        state.read(|state| state.find_meta("active").unwrap().pull_requests.len()),
        2
    );
    assert_eq!(
        state.read(|state| state.find_meta("settled").unwrap().pull_requests.len()),
        1,
        "settled threads receive the snapshot without installing siblings"
    );
    state.update(&mut cx, |state, cx| {
        let mut meta = state.find_meta("settled").unwrap();
        meta.settled_at = None;
        state.save_pull_request_meta(meta, cx);
        state.refresh_thread_pull_requests("settled", cx);
    });
    cx.run_until(|state| !state.pull_requests.syncing && state.pull_requests.requested.is_empty());
    assert_eq!(
        state.read(|state| state.find_meta("settled").unwrap().pull_requests.len()),
        2,
        "unchanged known topology expands when the thread becomes unsettled"
    );
    let key = PullRequestKey::new("github.com", "sample/project", 2);
    state.update(&mut cx, |state, cx| {
        let mut meta = state.find_meta("active").unwrap();
        meta.pull_requests
            .iter_mut()
            .find(|link| link.key == key)
            .unwrap()
            .watch = Some(json!({"id":"old-watch"}));
        state.save_pull_request_meta(meta, cx);
        state.unlink_pull_request("active", &key, cx);
        state.refresh_thread_pull_requests("active", cx);
    });
    cx.run_until(|state| !state.pull_requests.syncing && state.pull_requests.requested.is_empty());
    let dismissed = state.read(|state| {
        state
            .find_meta("active")
            .unwrap()
            .pull_requests
            .into_iter()
            .find(|link| link.key == key)
            .unwrap()
    });
    assert_eq!(
        serde_json::to_value(&dismissed).unwrap(),
        json!({"key":{"host":"github.com","repository":"sample/project","number":2},"source":"stack_dismissed"})
    );
    state.update(&mut cx, |state, cx| {
        state
            .apply_pull_request_link(
                "active",
                key.clone(),
                "https://github.com/sample/project/pull/2".into(),
                PullRequestSource::Manual,
                true,
                cx,
            )
            .unwrap()
    });
    cx.run_until(|state| !state.pull_requests.syncing && state.pull_requests.requested.is_empty());
    let restored = state.read(|state| {
        state
            .find_meta("active")
            .unwrap()
            .pull_requests
            .into_iter()
            .find(|link| link.key == key)
            .unwrap()
    });
    assert!(restored.visible() && restored.watch.is_none() && restored.snapshot.is_some());
    assert!(matches!(
        restored.stack,
        PullRequestStackState::Native(PullRequestStack { number: 7, .. })
    ));
}

#[test]
fn rate_limit_keeps_requests_due_until_host_pause_expires() {
    let store = TestStore::new("tcode-pr-paused");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let calls = Arc::new(Mutex::new(0));
    let serving = calls.clone();
    let _server = fixture.serve(move |exchange| {
        let mut calls = serving.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            exchange.reply(429, "retry-after: 1\r\n", b"{}");
        } else {
            exchange.reply(
                200,
                "",
                &serde_json::to_vec(&json!({"data":{"s0":{"pullRequest":pr(1,"OPEN",false)}}}))
                    .unwrap(),
            );
        }
    });
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, cx| {
        state.pull_requests = PullRequestRuntime::new(api);
        state.sessions.push(linked_meta("active", false));
        state.refresh_thread_pull_requests("active", cx);
    });
    cx.run_until(|state| !state.pull_requests.syncing && !state.pull_requests.paused.is_empty());
    assert!(state.read(|state| matches!(
        state.find_meta("active").unwrap().pull_requests[0].sync_error,
        Some(PullRequestSyncError::RateLimited { .. })
    )));
    sweep(&state, &mut cx);
    assert_eq!(*calls.lock().unwrap(), 1);
    assert!(state.read(|state| {
        state.find_meta("active").unwrap().pull_requests[0]
            .snapshot
            .is_none()
            && !state.pull_requests.requested.is_empty()
    }));
    state.update(&mut cx, |state, cx| {
        let until = *state.pull_requests.paused.values().next().unwrap();
        let host = cx.clone();
        cx.spawn_detached(async move {
            smol::Timer::after(until.duration_since(SystemTime::now()).unwrap_or_default()).await;
            host.enqueue(|state, cx| state.sweep_pull_requests(true, cx).detach());
        });
    });
    cx.run_until(|state| {
        state.find_meta("active").unwrap().pull_requests[0]
            .snapshot
            .is_some()
    });
    assert_eq!(*calls.lock().unwrap(), 2);
    assert!(state.read(|state| {
        state.pull_requests.requested.is_empty()
            && state.find_meta("active").unwrap().pull_requests[0]
                .sync_error
                .is_none()
    }));
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
fn discovery_answers_local_branches_and_drops_a_linked_worktree_result_after_branch_change() {
    let store = TestStore::new("tcode-pr-discovery");
    let root = store.root().join("checkout");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    git(
        &root,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:sample/project.git",
        ],
    );
    git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let worktree = store.root().join("worktree");
    git(
        &root,
        &["worktree", "add", "-b", "topic", worktree.to_str().unwrap()],
    );
    let cwd = worktree.join("subdirectory");
    std::fs::create_dir(&cwd).unwrap();
    assert_eq!(
        tcode_services::git::read_git_branch(&cwd).as_deref(),
        Some("topic")
    );
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let (arrived, requested) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let _server = fixture.serve(move |exchange| {
        let sent:Value=serde_json::from_slice(&exchange.body).unwrap();
        arrived.send(sent).unwrap();
        resume.recv_timeout(Duration::from_secs(5)).unwrap();
        exchange.reply(200,"",br#"{"data":{"repository":{"h0":{"nodes":[{"number":1,"url":"https://github.com/sample/project/pull/1","state":"OPEN","headRepositoryOwner":{"login":"SAMPLE"}}]}}}}"#);
    });
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        let mut meta = linked_meta("active", false);
        meta.pull_requests.clear();
        meta.cwd = cwd.clone();
        meta.worktree = Some(tcode_core::project::WorktreeInfo {
            root_project_path: root.clone(),
            base: "main".into(),
            branch: "topic".into(),
        });
        let project = Project::from_root(root.clone());
        meta.project_id = Some(project.id.clone());
        state.projects.push(project);
        state.sessions.push(meta);
    });
    state.update(&mut cx, |state, cx| {
        state.discover_pull_requests(None, false, cx).detach()
    });
    cx.run_until(|state| !state.pull_requests.discovering);
    assert!(
        requested.try_recv().is_err(),
        "an unpublished local branch makes no HTTP request"
    );
    git(&root, &["update-ref", "refs/remotes/origin/topic", "HEAD"]);
    state.update(&mut cx, |state, cx| {
        state.discover_pull_requests(None, true, cx).detach()
    });
    let sent = requested.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        sent["variables"]
            .as_object()
            .unwrap()
            .values()
            .any(|value| value == "topic")
    );
    git(&cwd, &["switch", "-c", "new-topic"]);
    release.send(()).unwrap();
    cx.run_until(|state| !state.pull_requests.discovering);
    assert!(
        state.read(|state| state.find_meta("active").unwrap().pull_requests.is_empty()),
        "a reply for the previous branch must not be linked"
    );
}

fn rpc(url: &str, token: &str, session: Option<&str>, body: Value) -> (Value, Option<String>) {
    let mut request = ureq::post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json, text/event-stream")
        .set("Content-Type", "application/json");
    if let Some(session) = session {
        request = request
            .set("Mcp-Session-Id", session)
            .set("MCP-Protocol-Version", "2025-11-25");
    }
    let response = request
        .send_bytes(&serde_json::to_vec(&body).unwrap())
        .unwrap();
    let session = response.header("Mcp-Session-Id").map(str::to_owned);
    let text = response.into_string().unwrap();
    let json = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .find(|data| !data.trim().is_empty())
        .unwrap_or(&text);
    (
        if json.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(json).unwrap()
        },
        session,
    )
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
    let mut settings = settings_store.load();
    settings.github.hosts.insert(
        "github.com".into(),
        tcode_core::settings::GitHubHostSettings {
            enabled: false,
            ..Default::default()
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
    let (commands, delivered) = smol::channel::unbounded();
    state.update(&mut cx, |state, cx| {
        let mut active = ActiveSession::new(state.find_meta("child").unwrap(), false, Vec::new());
        active.runtime = Runtime::Live(commands);
        active.meta = state.find_meta("child").unwrap();
        active.pull_request_tools = None;
        active.push_queued("continue".into(), Vec::new());
        assert_eq!(active.dispatch_next_pending(), Ok(false));
        state.install_selected(active);
        state.on_event(
            "child",
            AgentEvent::McpServersRegistered {
                names: vec!["tcode_pull_requests".into()],
            },
            cx,
        );
    });
    assert!(
        matches!(delivered.try_recv(), Ok(SessionCommand::SendTurn { text, .. }) if text.starts_with(LINKING_INSTRUCTIONS))
    );
    state.update(&mut cx, |state, cx| {
        let mut rejected = linked_meta("rejected", false);
        rejected.pull_requests.clear();
        state.pull_request_registration_for(&rejected).unwrap();
        let (sender, _receiver) = smol::channel::unbounded();
        let mut active = ActiveSession::new(rejected.clone(), false, Vec::new());
        active.runtime = Runtime::Live(sender);
        active.meta = rejected.clone();
        active.pull_request_tools = None;
        state.sessions.push(rejected);
        state.residents.live.insert("rejected".into(), active);
        state.on_event(
            "rejected",
            AgentEvent::McpServersRegistered { names: vec![] },
            cx,
        );
        assert!(
            !state
                .mcp
                .pull_request_registrations
                .contains_key("rejected")
        );
    });
    let result = Arc::new(Mutex::new(None));
    let completed = result.clone();
    let job = std::thread::spawn(move || {
        let (init, session) = rpc(
            &registration.url,
            &registration.bearer_token,
            None,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"contract-fixture","version":"1"}}}),
        );
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "initialize response: {init}"
        );
        rpc(
            &registration.url,
            &registration.bearer_token,
            session.as_deref(),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        );
        let call = |id, name, args| {
            rpc(&registration.url,&registration.bearer_token,session.as_deref(),json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})).0
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
        state
            .sessions
            .iter()
            .all(|meta| meta.pull_requests.is_empty())
    }));
}
