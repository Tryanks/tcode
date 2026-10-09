use super::*;
use crate::app::test_support::*;
use serde_json::{Value, json};
use std::sync::{Mutex, mpsc};
use tcode_core::pull_request::PullRequestSnapshot;
use tcode_protocol::{Command, EventEnvelope, HostMessage, RuntimeNotification, ServerEvent};
use tcode_services::{
    github::{Credentials, GitHub, GitHubApi},
    settings::SettingsStore,
};

use crate::app::test_support::github_fixture as fixture;

const START: &str = "2026-01-01T00:00:00.000Z";
const DETAIL: &str = "PullRequestWatchDetail";
const ACTIVITY: &str = "PullRequestWatchActivity";
const THREADS: &str = "PullRequestWatchThreads";
const TAIL: &str = "PullRequestWatchThreadComments";
const FINGERPRINTS: &str = "PullRequestWatchFingerprints";

struct ReviewThread {
    id: &'static str,
    comments: Vec<Value>,
}

/// GitHub as the watch reads it, for pull request #1 of sample/project.
struct Host {
    state: &'static str,
    head: &'static str,
    checks: Vec<Value>,
    mergeable: &'static str,
    comments: Vec<Value>,
    /// The comment list always has another page.
    endless_comments: bool,
    threads: Vec<ReviewThread>,
    fingerprints: bool,
    rate_limited: Option<&'static str>,
    failing: Option<&'static str>,
    /// Holds the next detail read until the test answers it.
    gate: Option<mpsc::Receiver<()>>,
    requests: Vec<String>,
}

fn check(name: &str, conclusion: &str) -> Value {
    json!({
        "__typename": "CheckRun",
        "name": name,
        "status": if conclusion == "PENDING" { "IN_PROGRESS" } else { "COMPLETED" },
        "conclusion": if conclusion == "PENDING" { Value::Null } else { json!(conclusion) },
        "detailsUrl": format!("https://ci.test/{name}"),
    })
}
fn comment(id: &str, author: &str, at: &str) -> Value {
    json!({
        "id": id,
        "body": format!("note {id}"),
        "createdAt": at,
        "lastEditedAt": null,
        "url": format!("https://github.com/sample/project/pull/1#{id}"),
        "author": {"login": author},
    })
}
fn reply(host: &Mutex<Host>, exchange: fixture::Exchange) {
    let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
    let operation = sent["query"]
        .as_str()
        .unwrap()
        .split([' ', '('])
        .nth(1)
        .unwrap()
        .to_owned();
    let gate = {
        let mut host = host.lock().unwrap();
        host.requests.push(operation.clone());
        if operation == DETAIL {
            host.gate.take()
        } else {
            None
        }
    };
    if let Some(gate) = gate {
        let _ = gate.recv();
    }
    let host = host.lock().unwrap();
    if host
        .rate_limited
        .is_some_and(|limited| limited == operation || limited == "*")
    {
        return exchange.reply(
            403,
            "x-ratelimit-remaining: 0\r\nx-ratelimit-reset: 4102444800\r\n",
            br#"{"message":"API rate limit exceeded"}"#,
        );
    }
    if host.failing == Some(operation.as_str()) {
        return exchange.reply(502, "", b"{}");
    }
    let page = |nodes: &[Value], more: bool| {
        json!({
            "pageInfo": {"hasNextPage": more, "endCursor": more.then(|| format!("c{}", host.requests.len()))},
            "nodes": nodes,
        })
    };
    let data = match operation.as_str() {
        FINGERPRINTS => {
            let mut counts = HashMap::<String, u64>::new();
            for check in &host.checks {
                let state = check["conclusion"]
                    .as_str()
                    .or_else(|| check["status"].as_str())
                    .unwrap();
                *counts.entry(state.into()).or_default() += 1;
            }
            let edited = |rows: &[Value]| {
                rows.iter()
                    .map(|row| json!({"lastEditedAt": row["lastEditedAt"]}))
                    .collect::<Vec<_>>()
            };
            let mut data = serde_json::Map::new();
            for name in sent["variables"].as_object().unwrap().keys() {
                if let Some(alias) = name.strip_suffix("_number") {
                    let pull_request = host.fingerprints.then(|| {
                        json!({
                            "state": host.state,
                            "mergeable": host.mergeable,
                            "headRefOid": host.head,
                            "comments": {"totalCount": host.comments.len(), "nodes": edited(&host.comments)},
                            "reviews": {"totalCount": 0, "nodes": []},
                            "reviewThreads": {"totalCount": host.threads.len()},
                            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
                                "checkRunCountsByState": counts.iter().map(|(state, count)| json!({"state": state, "count": count})).collect::<Vec<_>>(),
                                "statusContextCountsByState": [],
                            }}}}]},
                        })
                    });
                    data.insert(alias.into(), json!({ "pullRequest": pull_request }));
                }
            }
            Value::Object(data)
        }
        DETAIL => json!({
            "viewer": {"login": "tcode-bot"},
            "repository": {"pullRequest": {
                "number": 1,
                "state": host.state,
                "mergedAt": (host.state == "MERGED").then_some("2026-01-02T00:00:00Z"),
                "mergeable": host.mergeable,
                "headRefOid": host.head,
                "baseRefName": "main",
                "author": {"login": "tcode-bot"},
                "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": page(&host.checks, false)}}}]},
            }},
        }),
        ACTIVITY => json!({"repository": {"pullRequest": {
            "comments": page(&host.comments, host.endless_comments),
            "reviews": page(&[], false),
        }}}),
        THREADS => json!({"repository": {"pullRequest": {"reviewThreads": page(
            &host
                .threads
                .iter()
                .map(|thread| json!({
                    "id": thread.id,
                    "path": "src/lib.rs",
                    "comments": {
                        "totalCount": thread.comments.len(),
                        "pageInfo": {"hasNextPage": thread.comments.len() > 10, "endCursor": "first"},
                        "nodes": thread.comments[..thread.comments.len().min(10)],
                    },
                }))
                .collect::<Vec<_>>(),
            false,
        )}}}),
        TAIL => {
            let thread = host
                .threads
                .iter()
                .find(|thread| sent["variables"]["thread"] == thread.id)
                .unwrap();
            json!({"node": {"comments": page(&thread.comments[10..], false)}})
        }
        _ => json!({}),
    };
    exchange.reply(
        200,
        "",
        &serde_json::to_vec(&json!({ "data": data })).unwrap(),
    );
}

struct Harness {
    dir: TestStore,
    store: SessionStore,
    host: Arc<Mutex<Host>>,
    cx: TestAppContext,
    state: TestEntity,
    commands: smol::channel::Receiver<SessionCommand>,
    events: smol::channel::Sender<AgentEvent>,
    _server: fixture::Server,
}

impl Harness {
    fn new(prefix: &str) -> Self {
        let dir = TestStore::new(prefix);
        let store = (*dir).clone();
        Self::open(dir, store, Arc::new(Mutex::new(Host::default())))
    }

    /// Close this host and open a fresh one over the same data, as a restart does.
    fn restart(mut self) -> Self {
        self.state
            .update(&mut self.cx, |state, _| state.close_store())
            .unwrap();
        let Self { dir, host, .. } = self;
        let store = SessionStore::open_at(dir.root().clone()).unwrap();
        let mut restarted = Self::open(dir, store, host);
        let sessions = restarted.store.load_index().unwrap();
        restarted
            .state
            .update(&mut restarted.cx, |state, _| state.sessions = sessions);
        restarted
    }

    fn open(dir: TestStore, store: SessionStore, host: Arc<Mutex<Host>>) -> Self {
        let fixture = fixture::Fixture::new();
        let api = GitHub::new(GitHubApi::new(
            Credentials::new(
                SettingsStore::new(store.root().to_path_buf()),
                [("GH_TOKEN".into(), "fixture".into())],
            ),
            fixture.builder(),
        ));
        let serving = host.clone();
        let server = fixture.serve(move |exchange| reply(&serving, exchange));
        let scripted = scripted_provider(ProviderKind::Codex);
        let mut cx = TestAppContext::default();
        let state = cx.new_entity({
            let mut state = TestClientState::new(store.clone());
            state.set_provider_launcher_for_test(scripted.launcher);
            state.pull_request_watches = WatchRuntime::new(api);
            state
        });
        Self {
            dir,
            store,
            host,
            cx,
            state,
            commands: scripted.commands,
            events: scripted.events,
            _server: server,
        }
    }

    fn host(&self) -> std::sync::MutexGuard<'_, Host> {
        self.host.lock().unwrap()
    }

    fn watch_thread(&mut self, id: &str, started_at: &str) {
        let mut meta = SessionMeta::new(
            ProviderKind::Codex,
            PathBuf::from("/tmp/synthetic-checkout"),
            None,
        );
        meta.id = id.into();
        meta.project_id = Some("sample".into());
        pull_request::link_pull_request(
            &mut meta.pull_requests,
            key(),
            "https://github.com/sample/project/pull/1".into(),
            PullRequestSource::Manual,
            1,
            true,
        );
        let mut watch = PullRequestWatch::new(0);
        watch.started_at = started_at.into();
        watch.remarks_through = started_at.into();
        meta.pull_requests[0].watch = Some(watch);
        self.store.upsert_meta(&meta).unwrap();
        self.state
            .update(&mut self.cx, |state, _| state.sessions.push(meta));
    }

    fn pass(&mut self) {
        self.state.update(&mut self.cx, |state, cx| {
            state.sweep_pull_request_watches(cx).detach()
        });
        self.cx
            .run_until(|state| !state.pull_request_watches.passing);
    }

    fn watch(&self, id: &str) -> Option<PullRequestWatch> {
        self.state
            .read(|state| state.find_meta(id).unwrap().pull_requests[0].watch.clone())
    }

    /// Every request since the last call.
    fn requests(&self) -> Vec<String> {
        std::mem::take(&mut self.host().requests)
    }

    /// The text of the next turn the scripted provider receives.
    fn delivered(&mut self) -> (u64, String) {
        let commands = self.commands.clone();
        self.cx.run_until(|_| !commands.is_empty());
        match commands.try_recv() {
            Ok(SessionCommand::SendTurn {
                delivery_id, text, ..
            }) => (delivery_id, text),
            other => panic!("expected a delivered turn, got {other:?}"),
        }
    }

    /// The provider takes the turn and finishes it.
    fn accept(&mut self, delivery_id: u64) {
        for event in [
            AgentEvent::TurnAccepted { delivery_id },
            AgentEvent::TurnCompleted {
                turn_id: "turn".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ] {
            self.events.try_send(event).unwrap();
        }
        self.cx.run_until_parked();
    }

    fn notices(&mut self) -> Vec<WatchNotice> {
        self.cx
            .drain_outgoing()
            .into_iter()
            .filter_map(|message| match message {
                HostMessage::Event(EventEnvelope {
                    event:
                        ServerEvent::Runtime(RuntimeNotification::Toast(
                            RuntimeToast::PullRequestWatch { notice, .. },
                        )),
                    ..
                }) => Some(notice),
                _ => None,
            })
            .collect()
    }
}

impl Default for Host {
    fn default() -> Self {
        Self {
            state: "OPEN",
            head: "1111111aaaa",
            checks: Vec::new(),
            mergeable: "MERGEABLE",
            comments: Vec::new(),
            endless_comments: false,
            threads: Vec::new(),
            fingerprints: true,
            rate_limited: None,
            failing: None,
            gate: None,
            requests: Vec::new(),
        }
    }
}

fn key() -> PullRequestKey {
    PullRequestKey::new("github.com", "sample/project", 1)
}

#[test]
fn rate_limited_passes_wake_nobody_and_end_nothing() {
    let mut harness = Harness::new("tcode-watch-rate-limit");
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.host().rate_limited = Some(DETAIL);
    harness.watch_thread("thread", START);
    for _ in 0..20 {
        harness.pass();
    }
    let requests = harness.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|operation| *operation == FINGERPRINTS)
            .count(),
        1,
        "the host's pause skips later passes without a request: {requests:?}"
    );
    let watch = harness.watch("thread").unwrap();
    assert!(watch.pending_wake.is_none() && watch.failed_checks.is_empty());
    assert!(
        harness
            .state
            .read(|state| state.pull_request_watches.failures.is_empty()),
        "a rate limit never counts toward giving up"
    );
    assert!(harness.commands.is_empty());
    assert!(harness.notices().is_empty());
}

#[test]
fn eight_failed_reads_end_the_watch_with_a_notice_delivered_before_it_ends() {
    let mut harness = Harness::new("tcode-watch-unreadable");
    harness.host().failing = Some(DETAIL);
    harness.watch_thread("thread", START);
    for _ in 0..7 {
        harness.pass();
    }
    assert!(harness.watch("thread").unwrap().pending_wake.is_none());
    harness.pass();
    let ending = harness.watch("thread").unwrap().pending_wake.unwrap();
    assert!(ending.last);
    let (delivery, text) = harness.delivered();
    assert_eq!(
        text,
        "Tcode stopped watching pull request #1 (https://github.com/sample/project/pull/1) because it failed to read it from the host 8 times in a row. Check it yourself, and call watch_pull_request to watch it again."
    );
    assert_eq!(harness.notices(), [WatchNotice::Unreadable]);
    harness.requests();
    harness.pass();
    assert!(
        harness.requests().is_empty(),
        "an ending watch is not read again"
    );
    harness.accept(delivery);
    assert_eq!(harness.watch("thread"), None);
}

#[test]
fn closed_ends_with_a_notice_and_merged_ends_silently() {
    let mut harness = Harness::new("tcode-watch-terminal");
    harness.host().state = "CLOSED";
    harness.watch_thread("thread", START);
    harness.pass();
    let (delivery, text) = harness.delivered();
    assert_eq!(
        text,
        "Pull request #1 (https://github.com/sample/project/pull/1) was closed, so Tcode stopped watching it. Call watch_pull_request if it reopens."
    );
    assert_eq!(harness.notices(), [WatchNotice::Closed]);
    harness.accept(delivery);
    assert_eq!(harness.watch("thread"), None);

    harness.host().state = "MERGED";
    harness.watch_thread("merged", START);
    harness.pass();
    assert_eq!(harness.watch("merged"), None);
    assert!(harness.notices().is_empty());
    assert!(harness.commands.is_empty(), "a merge wakes nobody");
}

#[test]
fn two_threads_share_one_read_with_separate_watermarks() {
    let mut harness = Harness::new("tcode-watch-shared");
    harness.host().comments = vec![comment("c1", "reviewer", "2026-01-01T00:05:00Z")];
    harness.watch_thread("early", START);
    harness.watch_thread("late", "2026-01-01T00:10:00.000Z");
    harness.pass();
    let requests = harness.requests();
    for operation in [FINGERPRINTS, DETAIL, ACTIVITY] {
        assert_eq!(
            requests.iter().filter(|sent| *sent == operation).count(),
            1,
            "{operation} once for both threads: {requests:?}"
        );
    }
    let early = harness.watch("early").unwrap();
    assert!(early.pending_wake.is_some());
    assert_eq!(early.remark_ids, ["c1"]);
    let late = harness.watch("late").unwrap();
    assert!(
        late.pending_wake.is_none(),
        "a remark from before its start is old news to the later thread"
    );
    assert_eq!(late.remarks_through, "2026-01-01T00:10:00.000Z");
}

#[test]
fn reads_follow_the_fingerprint_and_its_half_hour_backstop() {
    let mut harness = Harness::new("tcode-watch-fingerprint");
    harness.watch_thread("thread", START);
    harness.pass();
    harness.requests();
    harness.pass();
    assert_eq!(
        harness.requests(),
        [FINGERPRINTS],
        "a quiet pass makes only the fingerprint request"
    );
    harness.host().checks = vec![check("build", "SUCCESS"), check("lint", "FAILURE")];
    harness.pass();
    assert_eq!(
        harness.requests(),
        [FINGERPRINTS, DETAIL],
        "moved checks need the detail alone"
    );
    let (delivery, text) = harness.delivered();
    assert!(text.contains("- Checks failed on 1111111:\n  - lint https://ci.test/lint"));
    harness.accept(delivery);
    harness.host().checks.push(check("deploy", "PENDING"));
    harness.pass();
    harness.requests();
    harness.pass();
    assert_eq!(
        harness.requests(),
        [FINGERPRINTS, DETAIL],
        "a check still running is read again though the fingerprint is quiet"
    );
    harness.state.update(&mut harness.cx, |state, _| {
        for read in state.pull_request_watches.last_reads.values_mut() {
            read.at -= FINGERPRINT_REREAD;
        }
    });
    harness.pass();
    let requests = harness.requests();
    assert!(
        requests.contains(&ACTIVITY.to_owned()),
        "half an hour after the last activity read it is read again: {requests:?}"
    );
}

#[test]
fn without_a_fingerprint_the_snapshot_gates_reads_with_a_ten_minute_backstop() {
    let mut harness = Harness::new("tcode-watch-snapshot");
    harness.host().fingerprints = false;
    harness.watch_thread("thread", START);
    harness.pass();
    harness.requests();
    harness.pass();
    assert_eq!(harness.requests(), [FINGERPRINTS]);
    harness.state.update(&mut harness.cx, |state, _| {
        for read in state.pull_request_watches.last_reads.values_mut() {
            read.at -= QUIET_REREAD;
        }
    });
    harness.pass();
    let requests = harness.requests();
    assert!(
        requests.contains(&DETAIL.to_owned()) && requests.contains(&ACTIVITY.to_owned()),
        "{requests:?}"
    );
}

#[test]
fn a_wake_reaches_a_non_resident_thread_and_a_restart_before_acceptance_redelivers_it() {
    let mut harness = Harness::new("tcode-watch-redeliver");
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.watch_thread("thread", START);
    assert!(
        harness
            .state
            .read(|state| state.resident("thread").is_none())
    );
    harness.pass();
    let pending = harness.watch("thread").unwrap().pending_wake.unwrap();
    let (_, text) = harness.delivered();
    assert_eq!(text, pending.text);
    assert!(text.starts_with(
        "Update on pull request #1 (https://github.com/sample/project/pull/1), which Tcode is watching for you:\n- Checks failed on 1111111:"
    ));
    assert_eq!(
        harness.store.load_index().unwrap()[0].pull_requests[0]
            .watch
            .as_ref()
            .unwrap()
            .failed_checks,
        ["build"],
        "the watermark is written with the wake"
    );

    let mut harness = harness.restart();
    harness.requests();
    harness.pass();
    let (delivery, text) = harness.delivered();
    assert_eq!(text, pending.text, "the same wake, once");
    assert!(
        !harness.host().requests.contains(&DETAIL.to_owned()),
        "nothing is read while the last news waits for its turn"
    );
    harness.accept(delivery);
    let watch = harness.watch("thread").unwrap();
    assert_eq!(watch.pending_wake, None);
    let recorded = harness
        .store
        .read_events("thread")
        .unwrap()
        .into_iter()
        .find(|record| {
            matches!(
                record.event,
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::UserMessage { .. },
                    ..
                })
            )
        })
        .unwrap();
    assert_eq!(
        recorded.origin,
        Some(MessageOrigin::Server),
        "a wake is never a human message"
    );
    harness.pass();
    assert!(
        harness.commands.is_empty(),
        "a delivered wake is not repeated"
    );
}

#[test]
fn stop_discards_a_read_in_flight_and_refuses_a_late_agent_start() {
    let mut harness = Harness::new("tcode-watch-stop");
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.watch_thread("thread", START);
    harness.state.update(&mut harness.cx, |state, cx| {
        state.select_session("thread", cx);
        state.mcp.pull_request_registrations.insert(
            "thread".into(),
            (
                agent::McpRegistration {
                    name: "tcode_pull_requests".into(),
                    url: "http://127.0.0.1/pull-requests".into(),
                    bearer_token: "token".into(),
                },
                "GitHub".into(),
            ),
        );
    });
    harness.state.dispatch_command(
        &mut harness.cx,
        1,
        Command::SendTurn {
            session_id: "thread".into(),
            text: "Open a pull request and watch it.".into(),
            attachment_paths: Vec::new(),
        },
    );
    let (delivery_id, _) = harness.delivered();
    harness
        .events
        .try_send(AgentEvent::TurnAccepted { delivery_id })
        .unwrap();
    harness.cx.run_until_parked();

    let (release, gate) = mpsc::channel();
    harness.host().gate = Some(gate);
    harness.state.update(&mut harness.cx, |state, cx| {
        state.sweep_pull_request_watches(cx).detach()
    });
    let host = harness.host.clone();
    harness
        .cx
        .run_until(|_| host.lock().unwrap().requests.contains(&DETAIL.to_owned()));
    let (reply, answer) = async_channel::bounded(1);
    harness.state.update(&mut harness.cx, |state, cx| {
        state.handle_pull_request_request(
            pull_request_mcp::BrokerRequest {
                session_id: "thread".into(),
                operation: pull_request_mcp::Operation::Watch(pull_request_mcp::Target {
                    url: Some("https://github.com/sample/project/pull/1".into()),
                    repository: None,
                    number: None,
                    host: None,
                }),
                reply,
            },
            cx,
        );
    });
    harness.state.dispatch_command(
        &mut harness.cx,
        2,
        Command::Interrupt {
            session_id: "thread".into(),
        },
    );
    assert_eq!(harness.watch("thread"), None);
    release.send(()).unwrap();
    harness
        .cx
        .run_until(|state| !state.pull_request_watches.passing && !answer.is_empty());
    let refused = answer.try_recv().unwrap().unwrap_err();
    assert!(refused.contains("stopped"), "{refused}");
    assert_eq!(
        harness.watch("thread"),
        None,
        "neither a late read nor a late agent start resurrects it"
    );
    let sent: Vec<_> = std::iter::from_fn(|| harness.commands.try_recv().ok()).collect();
    assert!(
        matches!(sent[..], [SessionCommand::Interrupt]),
        "Stop reaches the provider and nothing is delivered: {sent:?}"
    );
    assert!(harness.notices().is_empty());

    harness.state.dispatch_command(
        &mut harness.cx,
        3,
        Command::WatchPullRequest {
            session_id: "thread".into(),
            key: key(),
            watching: true,
        },
    );
    harness.cx.run_until_parked();
    assert!(
        harness.watch("thread").is_some(),
        "the user may start a watch after Stop"
    );
}

#[test]
fn incomplete_remarks_never_advance_the_watermark_but_checks_still_report() {
    let mut harness = Harness::new("tcode-watch-incomplete");
    harness.host().checks = vec![check("build", "FAILURE")];
    let mut replies: Vec<_> = (0..12)
        .map(|index| comment(&format!("r{index}"), "tcode-bot", "2026-01-01T00:01:00Z"))
        .collect();
    replies[11] = comment("r11", "reviewer", "2026-01-01T00:02:00Z");
    harness.host().threads = vec![ReviewThread {
        id: "thread-a",
        comments: replies,
    }];
    harness.host().failing = Some(TAIL);
    harness.watch_thread("thread", START);
    harness.pass();
    let watch = harness.watch("thread").unwrap();
    assert_eq!(watch.failed_checks, ["build"], "the failure is still told");
    assert_eq!(watch.remarks_through, START);
    let (delivery, text) = harness.delivered();
    assert!(!text.contains("new comment"));
    harness.accept(delivery);

    harness.host().failing = None;
    harness.pass();
    let (_, text) = harness.delivered();
    assert!(
        text.contains("- 1 new comment:\n  - reviewer on src/lib.rs: \"note r11\""),
        "the reply past the first page wakes once it can be read: {text}"
    );
}

#[test]
fn a_list_past_ten_pages_is_incomplete() {
    let mut harness = Harness::new("tcode-watch-pages");
    harness.host().checks = vec![check("build", "FAILURE")];
    harness.host().endless_comments = true;
    harness.host().comments = vec![comment("c1", "reviewer", "2026-01-01T00:05:00Z")];
    harness.watch_thread("thread", START);
    harness.pass();
    let requests = harness.requests();
    assert_eq!(
        requests.iter().filter(|sent| *sent == ACTIVITY).count(),
        10,
        "{requests:?}"
    );
    let watch = harness.watch("thread").unwrap();
    assert_eq!(
        watch.remarks_through, START,
        "the comment read so far is not news yet"
    );
    assert_eq!(watch.failed_checks, ["build"]);
    let (_, text) = harness.delivered();
    assert!(text.contains("- Checks failed") && !text.contains("new comment"));
}

#[test]
fn the_half_hour_reread_pages_cached_review_thread_tails_again() {
    let mut harness = Harness::new("tcode-watch-tails");
    let replies: Vec<_> = (0..12)
        .map(|index| comment(&format!("r{index}"), "tcode-bot", "2026-01-01T00:01:00Z"))
        .collect();
    harness.host().threads = vec![ReviewThread {
        id: "thread-a",
        comments: replies,
    }];
    harness.watch_thread("thread", START);
    harness.pass();
    assert!(harness.watch("thread").unwrap().pending_wake.is_none());
    harness.host().threads[0].comments[11] = {
        let mut edited = comment("r11", "reviewer", "2026-01-01T00:01:00Z");
        edited["lastEditedAt"] = json!("2026-01-01T00:20:00Z");
        edited
    };
    harness.state.update(&mut harness.cx, |state, _| {
        for read in state.pull_request_watches.last_reads.values_mut() {
            read.at -= FINGERPRINT_REREAD;
        }
    });
    harness.requests();
    harness.pass();
    assert!(harness.requests().contains(&TAIL.to_owned()));
    let (_, text) = harness.delivered();
    assert!(
        text.contains("reviewer on src/lib.rs: \"note r11\""),
        "an edit past a thread's first page is seen though its count stayed: {text}"
    );
}

#[test]
fn agent_starts_are_refused_where_the_parent_owns_the_pull_request_or_it_merged() {
    let mut harness = Harness::new("tcode-watch-refusals");
    harness.watch_thread("thread", START);
    let start = |harness: &mut Harness, edit: &dyn Fn(&mut SessionMeta)| {
        harness.state.update(&mut harness.cx, |state, cx| {
            let mut meta = state.find_meta("thread").unwrap();
            meta.pull_requests[0].watch = None;
            edit(&mut meta);
            state.save_pull_request_meta(meta, cx);
            state.watch_pull_request_from_agent(
                "thread",
                key(),
                "https://github.com/sample/project/pull/1".into(),
                true,
                cx,
            )
        })
    };
    let snapshot = |state| PullRequestSnapshot {
        state,
        title: "Change".into(),
        head_branch: "change".into(),
        base_branch: "main".into(),
        is_draft: false,
        updated_at: "2026-01-01T00:00:00Z".into(),
        synced_at: 1,
        closed_at: None,
        merged_at: None,
        author: None,
        additions: 0,
        deletions: 0,
        changed_files: 0,
        review_decision: None,
        checks_state: None,
        mergeability: Default::default(),
    };
    for (edit, refusal) in [
        (
            &(|meta: &mut SessionMeta| meta.parent_session_id = Some("lead".into()))
                as &dyn Fn(&mut SessionMeta),
            "Its parent thread owns the pull request",
        ),
        (
            &|meta: &mut SessionMeta| meta.settled_at = Some(1),
            "Unsettle",
        ),
        (
            &|meta: &mut SessionMeta| meta.archived_at = Some(1),
            "archived",
        ),
        (
            &|meta: &mut SessionMeta| {
                meta.pull_requests[0].snapshot = Some(snapshot(PullRequestState::Merged))
            },
            "merged",
        ),
    ] {
        let error = start(&mut harness, edit).unwrap_err();
        assert!(error.contains(refusal), "{error}");
        harness.state.update(&mut harness.cx, |state, cx| {
            let mut meta = state.find_meta("thread").unwrap();
            meta.parent_session_id = None;
            meta.settled_at = None;
            meta.archived_at = None;
            meta.pull_requests[0].snapshot = None;
            state.save_pull_request_meta(meta, cx);
        });
    }
    let started = start(&mut harness, &|meta| {
        meta.pull_requests[0].snapshot = Some(snapshot(PullRequestState::Closed))
    })
    .unwrap();
    assert_eq!(
        started["watching"], true,
        "a saved closed PR may have reopened"
    );
    assert_eq!(started["wasWatching"], false);

    harness.state.update(&mut harness.cx, |state, cx| {
        state.settle_session("thread", cx)
    });
    assert_eq!(harness.watch("thread"), None, "settling ends the watch");
    harness.state.update(&mut harness.cx, |state, cx| {
        state.unsettle_session("thread", cx)
    });
    assert_eq!(
        harness.watch("thread"),
        None,
        "un-settling never restarts it"
    );
}
