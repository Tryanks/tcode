use super::*;
use crate::app::test_support::*;
use control_mcp::{SendMessageParams, SendMode};
use tcode_core::control::{ControlScope, FollowUp};

const HOUR_MS: u64 = 3_600_000;

fn turn_ended() -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: "t".into(),
        status: TurnStatus::Completed,
        usage: None,
    }
}

/// A thread of `project` that the store held before the host started; its last turn ended
/// at `ended_at`, unix ms, when one is given.
fn stored(
    store: &TestStore,
    id: &str,
    project: &str,
    ended_at: Option<u64>,
    edit: impl FnOnce(&mut SessionMeta),
) {
    stored_thread(store, id, ProviderKind::ClaudeCode, store.root(), |meta| {
        meta.project_id = Some(project.into());
        meta.title = format!("Thread {id}");
        edit(meta);
    });
    if let Some(at) = ended_at {
        store.append_event(id, at, &turn_ended()).unwrap();
    }
}

/// One turn of `id` that ended at `ended_at` with the assistant saying `reply`.
fn stored_turn(store: &TestStore, id: &str, ended_at: u64, reply: &str) {
    let reply = AgentEvent::ItemCompleted(ThreadItem {
        id: "reply".into(),
        parent_item_id: None,
        content: ItemContent::AssistantMessage {
            text: reply.into(),
            summarized: false,
        },
    });
    for (ts, event) in [
        (
            ended_at - 1000,
            AgentEvent::TurnStarted {
                turn_id: "t".into(),
            },
        ),
        (ended_at - 500, reply),
        (ended_at, turn_ended()),
    ] {
        store.append_event(id, ts, &event).unwrap();
    }
}

/// A host started over `store`, once the activity it reads from each of `active`'s logs
/// has reached the client's index.
fn host(cx: &mut TestAppContext, store: &TestStore, active: &[&str]) -> (TestEntity, Providers) {
    let (state, providers) = played_host(cx, store);
    state.subscribe(cx, Topic::Index);
    cx.run_until_seen(|view| {
        active.iter().all(|id| {
            view.activity(id)
                .is_some_and(|activity| activity.last_activity_at.is_some())
        })
    });
    (state, providers)
}

/// One `tcode_control` call from the thread `caller`, answered the way its MCP server is.
fn call(
    state: &TestEntity,
    cx: &mut TestAppContext,
    op: ControlOp,
) -> Result<serde_json::Value, String> {
    let (reply, response) = smol::channel::bounded(1);
    state.update(cx, |state, cx| {
        state.handle_control_op("caller".into(), op, reply, cx)
    });
    cx.run_until_recv(&response)
}

fn ids(page: &serde_json::Value, key: &str) -> Vec<String> {
    page[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

/// `list_threads` lists the caller's project by default, hides orchestrated children and
/// archived threads unless asked, orders by last activity, or by creation where a thread has
/// none, and pages without gaps or repeats.
#[test]
fn list_threads_honours_scope_hides_children_and_archived_and_pages() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-list-threads");
    let now = cx.now_millis();
    stored(&store, "caller", "a", Some(now - 5 * HOUR_MS), |_| {});
    stored(&store, "newest", "a", Some(now - HOUR_MS), |_| {});
    stored(&store, "middle", "a", Some(now - 2 * HOUR_MS), |_| {});
    stored(&store, "settled", "a", Some(now - 3 * HOUR_MS), |meta| {
        meta.settled_at = Some(now / 1000);
    });
    stored(&store, "child", "a", Some(now - HOUR_MS / 2), |meta| {
        meta.parent_session_id = Some("caller".into());
    });
    stored(&store, "archived", "a", None, |meta| {
        meta.created_at = (now - 4 * HOUR_MS) / 1000;
        meta.archived_at = Some(now / 1000);
    });
    stored(&store, "elsewhere", "b", Some(now), |_| {});
    let (state, _providers) = host(
        cx,
        &store,
        &[
            "caller",
            "newest",
            "middle",
            "settled",
            "child",
            "elsewhere",
        ],
    );

    let page = call(
        &state,
        cx,
        ControlOp::ListThreads(ListThreadsParams::default()),
    )
    .unwrap();
    assert_eq!(
        ids(&page, "threads"),
        ["newest", "middle", "settled", "caller"]
    );
    assert_eq!(page["threads"][2]["state"], "settled");

    let page = call(
        &state,
        cx,
        ControlOp::ListThreads(ListThreadsParams {
            include_children: Some(true),
            states: Some(vec![ThreadState::Active, ThreadState::Archived]),
            ..ListThreadsParams::default()
        }),
    )
    .unwrap();
    assert_eq!(
        ids(&page, "threads"),
        ["child", "newest", "middle", "archived", "caller"]
    );

    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let page = call(
            &state,
            cx,
            ControlOp::ListThreads(ListThreadsParams {
                limit: Some(1),
                cursor: cursor.clone(),
                ..ListThreadsParams::default()
            }),
        )
        .unwrap();
        paged.extend(ids(&page, "threads"));
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(paged, ["newest", "middle", "settled", "caller"]);

    let page = call(
        &state,
        cx,
        ControlOp::ListThreads(ListThreadsParams {
            scope: Some(ControlScope::Host),
            ..ListThreadsParams::default()
        }),
    )
    .unwrap();
    assert_eq!(ids(&page, "threads")[0], "elsewhere");
}

/// For an unsettled thread, `get_thread` names why automatic settlement does not apply, and
/// gives the due time when it would.
#[test]
fn get_thread_explains_why_an_unsettled_thread_was_not_auto_settled() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-settlement-explanation");
    let idle_since = cx.now_millis();
    let ended = Some(idle_since);
    stored(&store, "caller", "a", ended, |_| {});
    stored(&store, "idle", "a", ended, |_| {});
    stored(&store, "unsettled", "a", ended, |meta| {
        meta.settled_override = Some(SettledOverride::Active);
    });
    stored(&store, "pinned", "a", ended, |meta| {
        meta.pinned_at = Some(idle_since / 1000);
    });
    stored(&store, "follow_up", "a", ended, |meta| {
        meta.follow_up = Some(FollowUp {
            since: idle_since / 1000,
            note: None,
        });
    });
    stored(&store, "queued", "a", ended, |_| {});
    cx.clock.advance(Duration::from_millis(24 * HOUR_MS));
    let threads = [
        "caller",
        "idle",
        "unsettled",
        "pinned",
        "follow_up",
        "queued",
    ];
    let (state, providers) = host(cx, &store, &threads);
    // Another thread's message waits in `queued` for its time.
    providers.hold();
    let later = ControlOp::SendMessage(SendMessageParams {
        thread_ids: vec!["queued".into()],
        text: "Pick this up tomorrow".into(),
        mode: SendMode::Schedule {
            at: cx.now_secs() + 24 * 3600,
        },
        attachments: None,
        request_id: None,
    });
    call(&state, cx, later).unwrap();
    let explain = |cx: &mut TestAppContext, id: &str| {
        call(
            &state,
            cx,
            ControlOp::GetThread(GetThreadParams {
                thread_id: id.into(),
            }),
        )
        .unwrap()["settlement"]
            .clone()
    };

    let idle = explain(cx, "idle");
    assert_eq!(idle["would_auto_settle"], true);
    assert_eq!(idle["reasons"], serde_json::json!([]));
    assert!(
        idle["due_at"]
            .as_u64()
            .is_some_and(|due| due > cx.now_millis()),
        "{idle}"
    );
    for (id, reason) in [
        ("unsettled", "override_active"),
        ("pinned", "pinned"),
        ("follow_up", "follow_up"),
    ] {
        let explanation = explain(cx, id);
        assert_eq!(explanation["would_auto_settle"], false, "{id}");
        assert_eq!(explanation["due_at"], serde_json::Value::Null, "{id}");
        assert_eq!(explanation["reasons"], serde_json::json!([reason]), "{id}");
    }
    let queued = explain(cx, "queued");
    assert_eq!(queued["would_auto_settle"], false);
    assert!(
        queued["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("pending_agent_message")),
        "{queued}"
    );
}

/// Left to itself `search` covers the caller's own project; a thread the call names is
/// searched wherever it is.
#[test]
fn search_covers_the_scope_and_any_thread_it_names_and_returns_anchors() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-search");
    for (id, project) in [("caller", "a"), ("same", "a"), ("other", "b")] {
        stored(&store, id, project, None, |_| {});
        store
            .append_event(
                id,
                1,
                &AgentEvent::TurnStarted {
                    turn_id: "t".into(),
                },
            )
            .unwrap();
        let reply = AgentEvent::ItemCompleted(ThreadItem {
            id: "reply".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: format!("{id} fixed the needle bug"),
                summarized: false,
            },
        });
        store.append_event(id, 2, &reply).unwrap();
    }
    let (state, _providers) = host(cx, &store, &[]);
    let search = |thread_ids: Option<Vec<String>>| {
        ControlOp::Search(SearchParams {
            query: "needle".into(),
            thread_ids,
            ..SearchParams::default()
        })
    };

    let answer = call(&state, cx, search(None)).unwrap();
    let mut anchors: Vec<&str> = answer["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["anchor"].as_str().unwrap())
        .collect();
    anchors.sort();
    assert_eq!(anchors, ["caller#1", "same#1"]);
    assert_eq!(answer["total"], 2);
    let hit = &answer["hits"][0];
    assert!(
        hit["snippet"]
            .as_str()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .contains(hit["anchor"].as_str().unwrap()),
        "a snippet is fenced as data under its anchor: {hit}"
    );

    let answer = call(&state, cx, search(Some(vec!["other".into()]))).unwrap();
    assert_eq!(answer["hits"][0]["anchor"], "other#1");
    assert_eq!(answer["total"], 1);
}

/// `review_candidates` changes nothing, finds old unresolved work, and states its coverage.
#[test]
fn review_candidates_changes_no_state_and_reports_coverage() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-review");
    let now = cx.now_millis();
    let day = 24 * HOUR_MS;
    let old = now - 40 * day;
    // The user settles threads by hand here, so a thread idle for forty days is still open.
    std::fs::create_dir_all(store.root()).unwrap();
    std::fs::write(
        store.root().join("settings.json"),
        r#"{"settlement_automation":"suggest"}"#,
    )
    .unwrap();
    let mut project = Project::from_root(store.root().clone());
    project.id = "a".into();
    store.upsert_project(&project).unwrap();
    stored(&store, "caller", "a", Some(now), |_| {});
    stored(&store, "forgotten", "a", None, |_| {});
    stored_turn(
        &store,
        "forgotten",
        old,
        "Next step is wiring the importer.",
    );
    stored(&store, "unsettled", "a", None, |meta| {
        meta.settled_override = Some(SettledOverride::Active);
    });
    stored_turn(&store, "unsettled", now - 2 * day, "Still looking.");
    stored(&store, "long_settled", "a", Some(old), |meta| {
        meta.settled_at = Some(old / 1000);
    });
    stored(&store, "child", "a", Some(old), |meta| {
        meta.parent_session_id = Some("forgotten".into());
    });
    // A thread nothing was ever said in.
    stored(&store, "blank", "a", None, |meta| {
        meta.created_at = (old - day) / 1000;
    });
    let active = ["caller", "forgotten", "unsettled", "long_settled", "child"];
    let (state, _providers) = host(cx, &store, &active);
    let snapshot = |cx: &TestAppContext| {
        let mut threads: Vec<SessionMeta> = cx.seen().sessions().cloned().collect();
        threads.sort_by(|a, b| a.id.cmp(&b.id));
        let activity = active.map(|id| cx.seen().activity(id).cloned());
        (threads, activity)
    };
    let before = snapshot(cx);

    let review = call(
        &state,
        cx,
        ControlOp::ReviewCandidates(ReviewCandidatesParams::default()),
    )
    .unwrap();
    cx.run_until_parked();
    assert!(snapshot(cx) == before, "a review changes no state");

    assert_eq!(
        ids(&review, "candidates"),
        ["unsettled", "forgotten", "blank"]
    );
    let unsettled = &review["candidates"][0];
    assert_eq!(unsettled["manual_unsettle"], true);
    assert!(
        unsettled["settlement"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("override_active")),
        "{unsettled}"
    );
    let forgotten = &review["candidates"][1];
    assert_eq!(forgotten["idle_days"], 40);
    assert!(
        forgotten["last_turn"]["assistant_message"]
            .as_str()
            .unwrap()
            .contains("Next step is wiring the importer."),
        "{forgotten}"
    );
    let collapsed: Vec<(&str, &str)> = review["collapsed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["id"].as_str().unwrap(), row["reason"].as_str().unwrap()))
        .collect();
    assert_eq!(
        collapsed,
        [("caller", "just_active"), ("child", "orchestrate_child")]
    );
    assert_eq!(review["coverage"]["projects"], 1);
    assert_eq!(review["coverage"]["threads"], 6);
    assert_eq!(
        review["coverage"]["without_enough_data"], 1,
        "the thread with nothing in its log"
    );
}

/// `get_settings` shows the settings without environment values, keys this build does not
/// know, or token-shaped strings, and says how many it redacted.
#[test]
fn get_settings_leaves_out_env_values_unknown_keys_and_tokens() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-get-settings");
    stored(&store, "caller", "p", None, |_| {});
    // The settings file as an earlier run, of a newer build, left it.
    let mut settings = Settings::default();
    settings.profiles.insert(
        "team".into(),
        tcode_core::settings::ProviderProfile {
            kind: ProviderKind::Codex,
            settings: tcode_core::settings::ProviderSettings {
                env: vec![tcode_core::settings::EnvVar {
                    name: "BASE_URL".into(),
                    value: "https://private-endpoint.invalid".into(),
                    sensitive: false,
                }],
                ..tcode_core::settings::ProviderSettings::default()
            },
        },
    );
    settings.browser.home_url =
        Some("https://example.invalid/?t=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij".into());
    settings.unknown.insert(
        "future_secret".into(),
        serde_json::json!("from a newer build"),
    );
    tcode_services::settings::SettingsStore::new(store.root().clone())
        .save(&settings)
        .unwrap();
    let (state, _providers) = host(cx, &store, &[]);

    let answer = call(
        &state,
        cx,
        ControlOp::GetSettings(control_mcp::GetSettingsParams::default()),
    )
    .unwrap();
    let text = answer.to_string();
    assert!(!text.contains("private-endpoint"), "{text}");
    assert!(!text.contains("future_secret"), "{text}");
    assert!(!text.contains("ghp_ABCDEF"), "{text}");
    assert_eq!(
        answer["settings"]["profiles"]["team"]["env"][0]["name"],
        "BASE_URL"
    );
    assert_eq!(answer["redacted"], 1);
}
