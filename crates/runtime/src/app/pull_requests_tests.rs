use super::*;
use crate::app::test_support::*;
use serde_json::{Value, json};
use std::sync::Mutex;
use tcode_core::pull_request::{PullRequestReviewDraftEdit, PullRequestStack};
use tcode_protocol::Command;
use tcode_services::{github::Credentials, settings::SettingsStore};

use crate::app::test_support::github_fixture as fixture;

struct HostReply {
    state: &'static str,
    stack: bool,
    requests: Vec<Value>,
}
fn pr(number: u64, state: &str, stack: bool) -> Value {
    let terminal = chrono::Utc::now().to_rfc3339();
    json!({
        "number": number,
        "url": format!("https://github.com/sample/project/pull/{number}"),
        "title": format!("Change {number}"),
        "state": state,
        "isDraft": false,
        "headRefName": format!("layer-{number}"),
        "baseRefName": "main",
        "updatedAt": "2026-10-08T00:00:00Z",
        "mergedAt": (state == "MERGED").then_some(&terminal),
        "closedAt": (state != "OPEN").then_some(&terminal),
        "additions": 3,
        "deletions": 1,
        "changedFiles": 1,
        "reviewDecision": "APPROVED",
        "mergeable": "MERGEABLE",
        "stack": if stack { json!({"number": 7}) } else { Value::Null },
    })
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

fn linked(id: &str, number: u64, settled: bool) -> SessionMeta {
    let mut meta = linked_meta(id, settled);
    meta.pull_requests[0].key.number = number;
    meta.pull_requests[0].url = format!("https://github.com/sample/project/pull/{number}");
    meta
}
fn command(command: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: "command".into(),
        parent_item_id: None,
        content: ItemContent::CommandExecution {
            command: command.into(),
            output: String::new(),
            exit_code: Some(0),
            status: ItemStatus::Completed,
        },
    })
}
fn turn_completed() -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: "turn".into(),
        status: TurnStatus::Completed,
        usage: None,
    }
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
    let read_numbers = || -> Vec<u64> {
        response
            .lock()
            .unwrap()
            .requests
            .iter()
            .flat_map(|sent| {
                sent["variables"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .filter(|(name, _)| name.ends_with("_number"))
                    .map(|(_, number)| number.as_u64().unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        for meta in [
            linked("active", 1, false),
            linked("settled", 1, true),
            linked("settled-alone", 2, true),
        ] {
            store.upsert_meta(&meta).unwrap();
            state.sessions.push(meta);
        }
    });
    sweep(&state, &mut cx);
    assert_eq!(
        read_numbers(),
        vec![1],
        "a PR shared by two threads needs one read"
    );
    let first = store.load_index().unwrap();
    for id in ["active", "settled"] {
        assert!(first.iter().any(|meta| {
            meta.id == id
                && meta.pull_requests[0]
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.state == PullRequestState::Open)
        }));
    }
    sweep(&state, &mut cx);
    assert_eq!(
        store.load_index().unwrap(),
        first,
        "unchanged state must preserve activity, unread state and synced_at"
    );
    response.lock().unwrap().state = "CLOSED";
    state.update(&mut cx, |state, cx| {
        state.on_event("active", command("cd project&&gh pr close 1"), cx);
        state.on_event("active", turn_completed(), cx);
    });
    cx.run_until(|state| {
        !state.pull_requests.syncing
            && state.find_meta("active").unwrap().pull_requests[0]
                .snapshot
                .as_ref()
                .is_some_and(|s| s.state == PullRequestState::Closed)
    });
    let closed_reads = read_numbers().len();
    sweep(&state, &mut cx);
    assert_eq!(read_numbers().len(), closed_reads);
    let fifteen_minutes_pass = |state: &TestEntity, cx: &mut TestAppContext| {
        state.update(cx, |state, _| {
            state.pull_requests.last_synced.insert(
                PullRequestKey::new("github.com", "sample/project", 1),
                now_secs() - 900,
            );
        })
    };
    fifteen_minutes_pass(&state, &mut cx);
    response.lock().unwrap().state = "MERGED";
    sweep(&state, &mut cx);
    assert_eq!(read_numbers().len(), closed_reads + 1);
    assert!(state.read(|state| {
        state.find_meta("active").unwrap().pull_requests[0]
            .snapshot
            .as_ref()
            .is_some_and(|s| s.state == PullRequestState::Merged)
    }));
    fifteen_minutes_pass(&state, &mut cx);
    sweep(&state, &mut cx);
    assert_eq!(read_numbers().len(), closed_reads + 1, "merged stops reads");
    assert!(
        !read_numbers().contains(&2),
        "a settled thread's open or unsynced link nobody shares is not independently due"
    );
    assert!(state.read(|state| {
        state.find_meta("settled-alone").unwrap().pull_requests[0]
            .snapshot
            .is_none()
    }));
}

#[test]
fn merge_or_close_detection_matches_words_in_the_raw_command() {
    for command in [
        "gh pr merge 12 --squash",
        "cd repo && gh pr close 3",
        "(gh  pr\tmerge)",
        "glab mr merge 4",
    ] {
        assert!(merges_or_closes(command), "{command}");
    }
    for command in [
        "gh pr view 12",
        "ugh pr merge",
        "gh pr merged",
        "gh prmerge",
        "echo gh pr",
    ] {
        assert!(!merges_or_closes(command), "{command}");
    }
}

#[test]
fn native_stack_sync_preserves_dismissals_and_explicit_restore() {
    let store = TestStore::new("tcode-pr-stack");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let _server = fixture.serve(|exchange| {
        if exchange.request.starts_with("GET ") {
            exchange.reply(
                200,
                "",
                br#"[{"id":"stack-7","number":7,"url":"https://github.com/sample/project/stack/7","base":{"ref":"main"},"pull_requests":[{"number":1,"head":{"ref":"layer-1"},"state":"merged"},{"number":2,"head":{"ref":"layer-2"},"state":"open"}]}]"#,
            );
        } else {
            let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
            let mut data = serde_json::Map::new();
            for (name, number) in sent["variables"].as_object().unwrap() {
                if let Some(alias) = name.strip_suffix("_number") {
                    let state = if number == 1 { "MERGED" } else { "OPEN" };
                    data.insert(
                        alias.into(),
                        json!({"pullRequest": pr(number.as_u64().unwrap(), state, true)}),
                    );
                }
            }
            exchange.reply(200, "", &serde_json::to_vec(&json!({"data":data})).unwrap());
        }
    });
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        state
            .sessions
            .extend([linked_meta("active", false), linked("settled", 2, true)]);
    });
    let settle_requests = |cx: &mut TestAppContext| {
        cx.run_until(|state| {
            !state.pull_requests.syncing
                && !state.pull_requests.sync_scheduled
                && state.pull_requests.requested.is_empty()
        })
    };
    sweep(&state, &mut cx);
    settle_requests(&mut cx);
    assert_eq!(
        state.read(|state| state.find_meta("active").unwrap().pull_requests.len()),
        2
    );
    let settled = state.read(|state| state.find_meta("settled").unwrap().pull_requests);
    assert!(
        settled.len() == 1 && settled[0].snapshot.is_some(),
        "settled threads receive the snapshot without installing siblings"
    );
    state.update(&mut cx, |state, cx| {
        let mut meta = state.find_meta("settled").unwrap();
        meta.settled_at = None;
        state.save_pull_request_meta(meta, cx);
    });
    sweep(&state, &mut cx);
    settle_requests(&mut cx);
    assert_eq!(
        state.read(|state| state.find_meta("settled").unwrap().pull_requests.len()),
        2,
        "known topology expands on the next read once the thread is unsettled"
    );
    let key = PullRequestKey::new("github.com", "sample/project", 2);
    let layer = |state: &TestEntity| {
        state.read(|state| {
            state
                .find_meta("active")
                .unwrap()
                .pull_requests
                .into_iter()
                .find(|link| link.key == key)
                .unwrap()
        })
    };
    state.update(&mut cx, |state, cx| {
        state.unlink_pull_request("active", &key, cx);
    });
    sweep(&state, &mut cx);
    settle_requests(&mut cx);
    assert_eq!(
        serde_json::to_value(layer(&state)).unwrap(),
        json!({"key":{"host":"github.com","repository":"sample/project","number":2},"source":"dismissed"})
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
    settle_requests(&mut cx);
    let restored = layer(&state);
    assert!(restored.visible() && restored.snapshot.is_some());
    assert_eq!(restored.source, PullRequestSource::Manual);
    assert!(matches!(
        restored.stack,
        PullRequestStackState::Native(PullRequestStack { number: 7, .. })
    ));
}

#[test]
fn a_synced_terminal_pull_request_settles_only_its_thread_once_its_stack_allows() {
    let store = TestStore::new("tcode-pr-settlement");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let _server = fixture.serve(|exchange| {
        if exchange.request.starts_with("GET ") {
            exchange.reply(
                200,
                "",
                br#"[{"id":"stack-7","number":7,"url":"https://github.com/sample/project/stack/7","base":{"ref":"main"},"pull_requests":[{"number":3,"head":{"ref":"layer-3"},"state":"merged"},{"number":4,"head":{"ref":"layer-4"},"state":"open"}]}]"#,
            );
        } else {
            let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
            let mut data = serde_json::Map::new();
            for (name, number) in sent["variables"].as_object().unwrap() {
                if let Some(alias) = name.strip_suffix("_number") {
                    let number = number.as_u64().unwrap();
                    let state = if number == 4 { "OPEN" } else { "MERGED" };
                    data.insert(
                        alias.into(),
                        json!({"pullRequest": pr(number, state, number >= 3)}),
                    );
                }
            }
            exchange.reply(200, "", &serde_json::to_vec(&json!({"data":data})).unwrap());
        }
    });
    let worked_at = now_millis() - 3_600_000;
    let activity = tcode_core::settlement::ThreadActivity {
        last_message_at: Some(worked_at - 60_000),
        last_human_message_at: Some(worked_at - 60_000),
        last_run_started_at: Some(worked_at - 59_000),
        last_run_completed_at: Some(worked_at),
        failed: false,
        interrupted: false,
    };
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        for meta in [linked("merged", 1, false), linked("stacked", 3, false)] {
            store.upsert_meta(&meta).unwrap();
            state
                .thread_activity
                .insert(meta.id.clone(), activity.clone());
            state.sessions.push(meta);
        }
    });
    sweep(&state, &mut cx);
    cx.run_until(|state| {
        !state.pull_requests.syncing
            && !state.pull_requests.sync_scheduled
            && state.pull_requests.requested.is_empty()
    });
    let merged = state.read(|state| state.find_meta("merged").unwrap());
    assert!(merged.is_settled());
    assert_eq!(merged.settled_at, Some(worked_at / 1000));
    let stacked = state.read(|state| state.find_meta("stacked").unwrap());
    assert_eq!(stacked.pull_requests.len(), 2);
    assert!(
        !stacked.is_settled(),
        "the merged layer's open sibling is linked before it is evaluated"
    );
}

#[test]
fn rate_limit_keeps_requests_due_until_host_pause_expires() {
    let store = TestStore::new("tcode-pr-paused");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let unpaused_api = client(&store, &fixture);
    let calls = Arc::new(Mutex::new(0));
    let serving = calls.clone();
    let _server = fixture.serve(move |exchange| {
        let mut calls = serving.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            exchange.reply(429, "retry-after: 3600\r\n", b"{}");
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
    let key = PullRequestKey::new("github.com", "sample/project", 1);
    state.update(&mut cx, |state, cx| {
        state.pull_requests = PullRequestRuntime::new(api);
        let mut meta = linked_meta("active", false);
        meta.pull_requests.clear();
        state.sessions.push(meta);
        state
            .apply_pull_request_link(
                "active",
                key.clone(),
                "https://github.com/sample/project/pull/1".into(),
                PullRequestSource::Manual,
                true,
                cx,
            )
            .unwrap();
    });
    cx.run_until(|state| !state.pull_requests.syncing && !state.pull_requests.paused.is_empty());
    assert!(state.read(|state| matches!(
        state.find_meta("active").unwrap().pull_requests[0].sync_error,
        Some(PullRequestSyncError::RateLimited { retry_at }) if retry_at >= now_secs() + 3_500
    )));
    sweep(&state, &mut cx);
    assert_eq!(*calls.lock().unwrap(), 1, "a paused group makes no request");
    assert!(state.read(|state| {
        state.find_meta("active").unwrap().pull_requests[0]
            .snapshot
            .is_none()
            && state.pull_requests.requested.contains_key(&key)
    }));
    // The hour passes: both the runtime's group pause and the transport's host pause
    // (private to a client, so a client whose pause has elapsed) are behind the clock.
    state.update(&mut cx, |state, _| {
        for until in state.pull_requests.paused.values_mut() {
            *until = SystemTime::now() - Duration::from_secs(1);
        }
        state.pull_requests.service = PullRequests::new(unpaused_api);
    });
    sweep(&state, &mut cx);
    assert_eq!(*calls.lock().unwrap(), 2);
    assert!(state.read(|state| {
        let link = &state.find_meta("active").unwrap().pull_requests[0];
        link.snapshot.is_some()
            && link.sync_error.is_none()
            && state.pull_requests.requested.is_empty()
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

fn commit(cwd: &Path) {
    git(
        cwd,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "change",
        ],
    );
}
fn checkout(store: &TestStore) -> PathBuf {
    let root = store.root().join("checkout");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    commit(&root);
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
    root
}

#[test]
fn discovery_answers_local_branches_and_drops_a_linked_worktree_result_after_branch_change() {
    let store = TestStore::new("tcode-pr-discovery");
    let root = checkout(&store);
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
        let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
        arrived.send(sent).unwrap();
        resume.recv_timeout(Duration::from_secs(5)).unwrap();
        exchange.reply(
            200,
            "",
            br#"{"data":{"repository":{"h0":{"nodes":[{"number":1,"url":"https://github.com/sample/project/pull/1","state":"OPEN","headRepositoryOwner":{"login":"SAMPLE"}}]}}}}"#,
        );
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
        state.discover_pull_requests(None, cx).detach()
    });
    cx.run_until(|state| !state.pull_requests.discovering);
    assert!(
        requested.try_recv().is_err(),
        "an unpublished local branch makes no HTTP request"
    );
    git(&root, &["update-ref", "refs/remotes/origin/topic", "HEAD"]);
    state.update(&mut cx, |state, cx| {
        state.discover_pull_requests_for("active", true, cx)
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

#[test]
fn an_unlinked_discovered_pull_request_stays_unlinked_across_new_refs_and_restarts() {
    let store = TestStore::new("tcode-pr-durable-unlink");
    let root = checkout(&store);
    git(&root, &["switch", "-c", "topic"]);
    git(&root, &["update-ref", "refs/remotes/origin/topic", "HEAD"]);
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    let restarted_api = client(&store, &fixture);
    let reads = Arc::new(Mutex::new(0));
    let counting = reads.clone();
    let _server = fixture.serve(move |exchange| {
        let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
        if sent["query"].as_str().unwrap().contains("PullRequestsByHead") {
            *counting.lock().unwrap() += 1;
        }
        exchange.reply(
            200,
            "",
            br#"{"data":{"repository":{"h0":{"nodes":[{"number":1,"url":"https://github.com/sample/project/pull/1","state":"OPEN","headRepositoryOwner":{"login":"sample"}}]}}}}"#,
        );
    });
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        let mut meta = linked_meta("active", false);
        meta.pull_requests.clear();
        meta.cwd = root.clone();
        let project = Project::from_root(root.clone());
        meta.project_id = Some(project.id.clone());
        state.projects.push(project);
        state.sessions.push(meta);
    });
    let key = PullRequestKey::new("github.com", "sample/project", 1);
    let links =
        |state: &TestEntity| state.read(|state| state.find_meta("active").unwrap().pull_requests);
    let discover = |state: &TestEntity, cx: &mut TestAppContext, threads| {
        let before = *reads.lock().unwrap();
        state.update(cx, |state, cx| {
            state.discover_pull_requests(threads, cx).detach()
        });
        cx.run_until(|state| !state.pull_requests.discovering);
        assert_eq!(
            *reads.lock().unwrap(),
            before + 1,
            "discovery read the head"
        );
    };
    let refresh = || Some(HashMap::from([("active".to_owned(), true)]));
    discover(&state, &mut cx, refresh());
    assert_eq!(links(&state)[0].source, PullRequestSource::Created);
    state.update(&mut cx, |state, cx| {
        state.unlink_pull_request("active", &key, cx)
    });
    commit(&root);
    git(&root, &["update-ref", "refs/remotes/origin/topic", "HEAD"]);
    discover(&state, &mut cx, refresh());
    let dismissed = links(&state);
    assert!(dismissed.len() == 1 && dismissed[0].source == PullRequestSource::Dismissed);
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(restarted_api)
    });
    discover(&state, &mut cx, None);
    assert_eq!(
        links(&state),
        dismissed,
        "a fresh runtime does not relink it"
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

#[test]
fn registered_tools_prefix_each_turn_except_a_native_command() {
    let store = TestStore::new("tcode-pr-instructions");
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let mut host = mcp_host::Host::bind().unwrap();
    let server = pull_request_mcp::start(&mut host);
    state.update(&mut cx, |state, cx| {
        state.pump_pull_request_requests(Some(server), cx)
    });
    let review = |kind| ProviderCommand {
        name: "review".into(),
        description: None,
        kind,
    };
    for (id, typed, instructed) in [
        ("plain", "fix the build", true),
        ("slash", "/compact keep the plan", false),
        ("skill", "$review the diff", false),
    ] {
        let (commands, delivered) = smol::channel::unbounded();
        state.update(&mut cx, |state, cx| {
            let mut meta = SessionMeta::new(
                ProviderKind::ClaudeCode,
                PathBuf::from("/tmp/synthetic-checkout"),
                None,
            );
            meta.id = id.into();
            let registration = state.pull_request_registration_for(&meta);
            assert!(registration.is_some());
            let mut active = ActiveSession::new(meta.clone(), false, Vec::new());
            active.runtime = Runtime::Live(commands);
            active.provider_commands = vec![
                review(ProviderCommandKind::Command),
                review(ProviderCommandKind::Skill),
            ];
            active.push_queued(typed.into(), Vec::new());
            state.sessions.push(meta);
            state.install_selected(active);
            assert_eq!(state.dispatch_next_queued(id, cx), Ok(true));
        });
        let Ok(SessionCommand::SendTurn {
            text, delivery_id, ..
        }) = delivered.try_recv()
        else {
            panic!("expected a provider delivery")
        };
        let typed_wire = typed.replacen('$', "/", 1);
        if instructed {
            assert_eq!(
                text,
                format!("{}{typed}", pull_request::LINKING_INSTRUCTIONS)
            );
        } else {
            assert_eq!(text, typed_wire, "a native command stays at byte zero");
        }
        state.update(&mut cx, |state, cx| {
            state.on_event(id, AgentEvent::TurnAccepted { delivery_id }, cx)
        });
        cx.run_until_parked();
        let (recorded, context_len) = store
            .read_events(id)
            .unwrap()
            .into_iter()
            .find_map(|event| match event.event {
                AgentEvent::ItemCompleted(ThreadItem {
                    content:
                        ItemContent::UserMessage {
                            text, context_len, ..
                        },
                    ..
                }) => Some((text, context_len)),
                _ => None,
            })
            .unwrap();
        let disclosed = context_len.map(|len| &recorded[..len]);
        assert_eq!(&recorded[context_len.unwrap_or(0)..], typed);
        assert_eq!(
            disclosed.and_then(pull_request::strip_linking_instructions),
            instructed.then_some(""),
            "the disclosure holds exactly the pull request instructions"
        );
    }
}

#[test]
fn the_agent_tools_are_disclosed_before_the_first_turn() {
    let store = TestStore::new("tcode-pr-tools-disclosed");
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let mut host = mcp_host::Host::bind().unwrap();
    let server = pull_request_mcp::start(&mut host);
    state.update(&mut cx, |state, cx| {
        state.pump_pull_request_requests(Some(server), cx)
    });
    for (provider, disclosed) in [(ProviderKind::ClaudeCode, true), (ProviderKind::Pi, false)] {
        let tools = state.update(&mut cx, |state, _| {
            let meta = SessionMeta::new(provider, PathBuf::from("/tmp/synthetic-checkout"), None);
            let id = meta.id.clone();
            state.sessions.push(meta.clone());
            state.install_selected(ActiveSession::new(meta, false, Vec::new()));
            state
                .session_status_snapshot(&id)
                .unwrap()
                .pull_request_tools
        });
        assert_eq!(
            tools.is_some(),
            disclosed,
            "{provider:?}: shown exactly when its provider would receive the tools"
        );
    }
}

#[test]
fn reads_answer_only_a_linked_pull_request_and_a_synced_change_reads_it_fresh() {
    let store = TestStore::new("tcode-pr-read");
    let fixture = fixture::Fixture::new();
    let api = client(&store, &fixture);
    // The pull request's last update, and every GraphQL operation the host sent.
    let model = Arc::new(Mutex::new(("2026-10-08T00:00:00Z", Vec::<String>::new())));
    let responding = model.clone();
    let _server = fixture.serve(move |exchange| {
        let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
        let operation = sent["query"]
            .as_str()
            .unwrap()
            .split(['(', ' '])
            .nth(1)
            .unwrap();
        let mut model = responding.lock().unwrap();
        model.1.push(operation.to_owned());
        let reply = match operation {
            "PullRequestSummaries" => {
                let mut row = pr(1, "OPEN", false);
                row["updatedAt"] = json!(model.0);
                json!({"data": {"s0": {"pullRequest": row}}})
            }
            "PullRequestConversation" => json!({"data": {"repository": {"pullRequest": {
                "id": "PR_1", "body": "Description", "createdAt": "2026-10-01T00:00:00Z",
                "url": "https://github.com/sample/project/pull/1", "author": {"login": "octocat"},
                "comments": {"pageInfo": {"hasNextPage": false}, "nodes": []},
                "reviews": {"pageInfo": {"hasNextPage": false}, "nodes": []},
            }}}}),
            _ => json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                "pageInfo": {"hasNextPage": false}, "nodes": [],
            }}}}}),
        };
        exchange.reply(200, "", &serde_json::to_vec(&reply).unwrap());
    });
    let conversations = || {
        model
            .lock()
            .unwrap()
            .1
            .iter()
            .filter(|operation| *operation == "PullRequestConversation")
            .count()
    };
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        let meta = linked("active", 1, false);
        store.upsert_meta(&meta).unwrap();
        state.sessions.push(meta);
    });
    let mut next_id = 0;
    let mut read = |cx: &mut TestAppContext, number: u64| {
        next_id += 1;
        state.deliver(
            cx,
            next_id,
            tcode_protocol::ClientPayload::Query(tcode_protocol::Query::PullRequest {
                session_id: "active".into(),
                key: PullRequestKey::new("github.com", "sample/project", number),
                read: tcode_protocol::PullRequestRead::Conversation,
            }),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            if let Some(result) =
                cx.drain_outgoing()
                    .into_iter()
                    .find_map(|message| match message {
                        tcode_protocol::HostMessage::QueryResult { id, result }
                            if id == next_id =>
                        {
                            Some(result)
                        }
                        _ => None,
                    })
            {
                return result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the read was answered"
            );
        }
    };

    sweep(&state, &mut cx);
    assert!(read(&mut cx, 1).is_ok());
    assert!(read(&mut cx, 1).is_ok());
    assert_eq!(conversations(), 1, "a second read within the TTL is shared");
    assert_eq!(
        read(&mut cx, 2).unwrap_err().code,
        "pull_request_not_linked",
        "a pull request the thread does not link is not read through it"
    );
    assert_eq!(conversations(), 1);

    sweep(&state, &mut cx);
    read(&mut cx, 1).unwrap();
    assert_eq!(conversations(), 1, "an unchanged pull request stays shared");
    model.lock().unwrap().0 = "2026-10-09T00:00:00Z";
    state.update(&mut cx, |state, cx| {
        state.request_pull_request_sync(PullRequestKey::new("github.com", "sample/project", 1), cx)
    });
    cx.run_until(|state| {
        state.find_meta("active").unwrap().pull_requests[0]
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.updated_at == "2026-10-09T00:00:00Z")
    });
    read(&mut cx, 1).unwrap();
    assert_eq!(conversations(), 2, "a change the sync saw is read fresh");

    // The thread shows every layer of the native stack its link carries.
    state.update(&mut cx, |state, _| {
        let layer = |number: u64| tcode_core::pull_request::PullRequestStackLayer {
            url: format!("https://github.com/sample/project/pull/{number}"),
            number,
            head_branch: format!("layer-{number}"),
            state: PullRequestState::Open,
        };
        let meta = state
            .sessions
            .iter_mut()
            .find(|meta| meta.id == "active")
            .unwrap();
        meta.pull_requests[0].stack = PullRequestStackState::Native(PullRequestStack {
            id: "stack".into(),
            number: 7,
            url: "https://github.com/sample/project/stacks/7".into(),
            base: "main".into(),
            layers: vec![layer(1), layer(3)],
        });
    });
    assert!(
        read(&mut cx, 3).is_ok(),
        "a layer of the linked pull request's stack is read through the thread"
    );
    assert_eq!(
        read(&mut cx, 4).unwrap_err().code,
        "pull_request_not_linked",
        "a pull request in no stack the thread shows is still refused"
    );
    assert_eq!(conversations(), 3);
}

#[test]
fn a_review_draft_is_the_hosts_across_a_restart_and_a_moved_head_keeps_it_until_moved() {
    const BASE: &str = "3333333333333333333333333333333333333333";
    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";
    let dir = TestStore::new("tcode-pr-review");
    let fixture = fixture::Fixture::new();
    let api = client(&dir, &fixture);
    // The head GitHub reports, and every request line and body it saw.
    let model = Arc::new(Mutex::new((OLD, Vec::<(String, String)>::new())));
    let responding = model.clone();
    let _server = fixture.serve(move |exchange| {
        let line = exchange.request.lines().next().unwrap().to_owned();
        let diff = exchange.request.contains("application/vnd.github.diff");
        let body = String::from_utf8_lossy(&exchange.body).into_owned();
        let mut model = responding.lock().unwrap();
        model.1.push((line.clone(), body.clone()));
        let head = model.0;
        // Line 8 is rewritten at the new head; every other line reads the same.
        let text = (1..=12)
            .map(|line| match (line, head) {
                (8, NEW) => "changed".to_owned(),
                _ => format!("line {line}"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let reply = if diff {
            "diff --git a/src/lib.rs b/src/lib.rs\nindex 1111111..2222222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,10 +1,10 @@\n line 1\n".to_owned()
        } else if line.starts_with("GET /repos/sample/project/pulls/1 ") {
            json!({"base": {"sha": BASE}, "head": {"sha": head}, "changed_files": 1, "node_id": "PR_1"})
                .to_string()
        } else if line.starts_with("GET /repos/sample/project/contents/src/lib.rs?ref=") {
            if line.contains(NEW) {
                text
            } else {
                (1..=12)
                    .map(|line| format!("line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        } else if body.contains("PullRequestSummaries") {
            json!({"data": {"s0": {"pullRequest": pr(1, "OPEN", false)}}}).to_string()
        } else {
            json!({"id": 1}).to_string()
        };
        exchange.reply(200, "", reply.as_bytes());
    });
    let key = PullRequestKey::new("github.com", "sample/project", 1);
    let mut cx = TestAppContext::default();
    let state = cx.new_entity(TestClientState::new((*dir).clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api.clone());
        let meta = linked("active", 1, false);
        dir.upsert_meta(&meta).unwrap();
        state.sessions.push(meta);
    });
    let mut next_id = 0;
    let mut command = |state: &TestEntity, cx: &mut TestAppContext, command: Command| {
        next_id += 1;
        state.dispatch_command(cx, next_id, command);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            if let Some(result) =
                cx.drain_outgoing()
                    .into_iter()
                    .find_map(|message| match message {
                        tcode_protocol::HostMessage::Ack { id, result } if id == next_id => {
                            Some(result)
                        }
                        _ => None,
                    })
            {
                return result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the command was answered"
            );
        }
    };
    let edit = |edit| Command::EditPullRequestReviewDraft {
        session_id: "active".into(),
        key: key.clone(),
        edit,
    };
    let add = |line: u32| {
        edit(PullRequestReviewDraftEdit::AddComment {
            head: OLD.into(),
            revision: OLD.into(),
            path: "src/lib.rs".into(),
            side: tcode_core::session::ReviewSide::New,
            start_line: line,
            end_line: line,
            body: format!("Line {line}"),
        })
    };
    let drafts = |state: &TestEntity| {
        state.read(|state| state.find_meta("active").unwrap().pull_request_reviews)
    };
    command(&state, &mut cx, add(3)).unwrap();
    command(&state, &mut cx, add(8)).unwrap();
    assert_eq!(
        command(&state, &mut cx, add(11)).unwrap_err().code,
        "pull_request_not_in_diff",
        "a line outside every hunk is refused before GitHub would refuse the review"
    );
    command(
        &state,
        &mut cx,
        edit(PullRequestReviewDraftEdit::SetBody {
            body: "Two things".into(),
        }),
    )
    .unwrap();

    // A restart: a fresh host over the same data.
    state
        .update(&mut cx, |state, _| state.close_store())
        .unwrap();
    let store = SessionStore::open_at(dir.root().clone()).unwrap();
    let sessions = store.load_index().unwrap();
    let draft = sessions[0].pull_request_reviews.clone();
    assert_eq!(draft.len(), 1);
    assert_eq!(
        (
            draft[0].head.as_str(),
            draft[0].body.as_str(),
            draft[0]
                .comments
                .iter()
                .map(|comment| comment.end_line)
                .collect::<Vec<_>>()
        ),
        (OLD, "Two things", vec![3, 8])
    );
    let state = cx.new_entity(TestClientState::new(store.clone()));
    state.update(&mut cx, |state, _| {
        state.pull_requests = PullRequestRuntime::new(api);
        state.sessions = sessions;
    });
    let submit = |head: &str| Command::RunPullRequestAction {
        session_id: "active".into(),
        key: key.clone(),
        action: PullRequestAction::SubmitReview {
            verdict: tcode_protocol::PullRequestReviewVerdict::Comment,
            head: head.into(),
        },
    };
    let reviews = || {
        model
            .lock()
            .unwrap()
            .1
            .iter()
            .filter(|(line, _)| line.starts_with("POST /repos/sample/project/pulls/1/reviews "))
            .map(|(_, body)| serde_json::from_str::<Value>(body).unwrap())
            .collect::<Vec<_>>()
    };

    model.lock().unwrap().0 = NEW;
    assert_eq!(
        command(&state, &mut cx, submit(OLD)).unwrap(),
        CommandResponse::PullRequestAction(PullRequestActionResult::Rejected(
            tcode_protocol::PullRequestRejection::StaleHead { head: NEW.into() }
        ))
    );
    assert!(reviews().is_empty());
    assert_eq!(
        drafts(&state),
        draft,
        "a stale submission keeps the draft whole"
    );

    command(
        &state,
        &mut cx,
        edit(PullRequestReviewDraftEdit::MoveToHead),
    )
    .unwrap();
    let moved = drafts(&state);
    assert_eq!(moved[0].head, NEW);
    assert_eq!(
        moved[0]
            .comments
            .iter()
            .map(|comment| (comment.end_line, comment.placed, comment.revision.as_str()))
            .collect::<Vec<_>>(),
        vec![(3, true, NEW), (8, false, OLD)],
        "a comment whose line reads the same moves; one whose line changed is kept unplaced"
    );
    assert_eq!(
        command(&state, &mut cx, submit(NEW)).unwrap(),
        CommandResponse::PullRequestAction(PullRequestActionResult::Rejected(
            tcode_protocol::PullRequestRejection::Invalid
        )),
        "an unplaced comment is never sent"
    );
    let unplaced = moved[0].comments[1].id;
    command(
        &state,
        &mut cx,
        edit(PullRequestReviewDraftEdit::RemoveComment { id: unplaced }),
    )
    .unwrap();
    assert_eq!(
        command(&state, &mut cx, submit(NEW)).unwrap(),
        CommandResponse::PullRequestAction(PullRequestActionResult::Applied)
    );
    assert_eq!(
        reviews(),
        vec![json!({
            "commit_id": NEW,
            "event": "COMMENT",
            "body": "Two things",
            "comments": [{"path": "src/lib.rs", "line": 3, "side": "RIGHT", "body": "Line 3"}],
        })]
    );
    assert!(
        drafts(&state).is_empty(),
        "a submitted review leaves the draft"
    );
    cx.run_until(|state| !state.pull_requests.syncing && state.pull_requests.requested.is_empty());
    assert!(
        model
            .lock()
            .unwrap()
            .1
            .iter()
            .any(|(_, body)| body.contains("PullRequestSummaries")),
        "the sync reads the pull request after the write"
    );
    state
        .update(&mut cx, |state, _| state.close_store())
        .unwrap();
}
