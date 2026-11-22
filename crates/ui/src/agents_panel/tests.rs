use agent::ProviderKind;
use gpui::{AppContext as _, TestAppContext};
use tcode_core::project::SessionMeta;
use tcode_protocol::{EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic};

use super::Agents;
use crate::store::{WorkspaceAttachment, WorkspaceStore};

/// A settled child is not resident, so the host reports no activity for
/// it: it is settled and the badge leaves it out, while an unsettled
/// child the host has not reported yet is running.
#[gpui::test]
fn a_settled_child_without_reported_activity_is_settled(cx: &mut TestAppContext) {
    let (to_host, _requests) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    let thread = |id: &str, parent: Option<&str>| {
        let mut meta = SessionMeta::new(ProviderKind::Codex, "/project".into(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_string);
        meta
    };
    let mut settled = thread("settled", Some("lead"));
    settled.settled_at = Some(1);
    let index = tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
        request_id: None,
        topic: Topic::Index,
        event: ServerEvent::IndexSnapshot(IndexSnapshot {
            projects: Vec::new(),
            sessions: vec![
                thread("lead", None),
                settled,
                thread("unsettled", Some("lead")),
            ],
            summary: Default::default(),
        }),
    }))
    .unwrap();
    crate::store::tests::seed_full_scope(&store, &incoming, vec![index], cx);
    let mut pump = std::pin::pin!(link.pump());
    let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
    cx.run_until_parked();

    store.read_with(cx, |store, cx| {
        let agents = Agents::of(store, "lead", cx);
        let keys =
            |rows: &[super::AgentRow]| rows.iter().map(|row| row.key.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&agents.settled), ["settled"]);
        assert_eq!(keys(&agents.running), ["unsettled"]);
        assert_eq!(agents.count(), 1);
    });
    link.close();
}

/// A child the host reports waiting on an approval says so on its line,
/// ahead of how the lead's agents stand, and names its own branch.
#[gpui::test]
fn a_child_awaiting_approval_says_so_first(cx: &mut TestAppContext) {
    use tcode_core::project::WorktreeInfo;
    use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};
    use tcode_protocol::{AgentStatus, SessionActivity};
    let (to_host, _requests) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    let thread = |id: &str, parent: Option<&str>| {
        let mut meta = SessionMeta::new(ProviderKind::Codex, "/project".into(), None);
        meta.id = id.into();
        meta.parent_session_id = parent.map(str::to_owned);
        meta
    };
    let mut asking = thread("asking", Some("lead"));
    asking.model = Some("gpt-6".into());
    asking.worktree = Some(WorktreeInfo {
        root_project_path: "/project".into(),
        base: "main".into(),
        branch: "tcode/asking".into(),
    });
    let mut index = IndexSnapshot {
        sessions: vec![thread("lead", None), asking],
        projects: vec![],
        summary: Default::default(),
    };
    index.summary.activity.insert(
        "asking".into(),
        SessionActivity {
            agent: Some(AgentStatus {
                execution: AgentExecution::Waiting,
                delivery: AgentDelivery::Running,
                report: AgentReport::Expected,
                run_started_at: None,
                run_completed_at: None,
            }),
            working: false,
            turn_running: true,
            background: Default::default(),
            unsettled: 0,
            wakes: Default::default(),
            waiting_for_approval: true,
            waiting_for_input: false,
            settle_blocked: false,
            failed: false,
            unread: false,
            last_activity_at: None,
            fork: tcode_protocol::ForkAvailability::Available,
            latest_activity: None,
        },
    );
    let index = tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
        request_id: None,
        topic: Topic::Index,
        event: ServerEvent::IndexSnapshot(index),
    }))
    .unwrap();
    crate::store::tests::seed_full_scope(&store, &incoming, vec![index], cx);
    let mut driver = crate::store::tests::LocalHostDriver::from_link(link.clone());
    driver.wait_until(cx, "child status", |cx| {
        store.read_with(cx, |store, _| store.agent_status("asking").is_some())
    });

    store.read_with(cx, |store, cx| {
        let agents = Agents::of(store, "lead", cx);
        let lines = agents.lines();
        assert_eq!(
            lines[0].state,
            Some(super::AgentState::Attention(super::Attention::Approval))
        );
        assert_eq!(lines[0].model.as_deref(), Some("gpt-6"));
        assert_eq!(lines[0].branch.as_deref(), Some("tcode/asking"));
        assert_eq!(
            agents.summary(),
            Some(format!(
                "{} · {}",
                crate::tr!("agents.summary_approval_one"),
                crate::tr!("agents.summary_running_one")
            ))
        );
    });
    link.close();
}

/// What a running agent is doing comes from the host's index, on the
/// dispatched agent's thread or a native subagent's mirror, and gives way
/// to its result as it settles, even before the host's index catches up
/// with a native subagent the lead's transcript has seen end.
#[gpui::test]
fn a_running_agents_activity_gives_way_to_its_result(cx: &mut TestAppContext) {
    use agent::{AgentEvent, ItemContent, ItemStatus, ThreadItem};
    use gpui::{px, size};
    use tcode_core::session::LatestActivity;
    use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};
    use tcode_protocol::{AgentStatus, SessionActivity};
    cx.update(crate::test_client::init);
    let (to_host, _requests) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    let thread = |id: &str, parent: Option<&str>| {
        let mut meta = SessionMeta::new(ProviderKind::Codex, "/project".into(), None);
        meta.id = id.into();
        meta.title = id.into();
        meta.parent_session_id = parent.map(str::to_owned);
        meta
    };
    let mut mirror = thread("mirror", Some("lead"));
    mirror.native_subagent = Some("spawn".into());
    let activity =
        |agent: Option<AgentStatus>, latest_activity: Option<LatestActivity>| SessionActivity {
            agent,
            working: latest_activity.is_some(),
            turn_running: latest_activity.is_some(),
            background: Default::default(),
            unsettled: 0,
            wakes: Default::default(),
            waiting_for_approval: false,
            waiting_for_input: false,
            settle_blocked: false,
            failed: false,
            unread: false,
            last_activity_at: None,
            fork: tcode_protocol::ForkAvailability::Available,
            latest_activity,
        };
    let status = |execution, delivery, report| AgentStatus {
        execution,
        delivery,
        report,
        run_started_at: None,
        run_completed_at: None,
    };
    let command = LatestActivity::Command {
        text: "cargo test".into(),
    };
    let mut index = IndexSnapshot {
        sessions: vec![thread("lead", None), thread("worker", Some("lead")), mirror],
        projects: vec![],
        summary: Default::default(),
    };
    index.summary.activity.insert(
        "worker".into(),
        activity(
            Some(status(
                AgentExecution::Working,
                AgentDelivery::Running,
                AgentReport::Expected,
            )),
            Some(command.clone()),
        ),
    );
    index.summary.activity.insert(
        "mirror".into(),
        activity(None, Some(LatestActivity::Thinking)),
    );
    let line = |topic, event| {
        tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic,
            event,
        }))
        .unwrap()
    };
    crate::store::tests::seed_full_scope(
        &store,
        &incoming,
        vec![line(Topic::Index, ServerEvent::IndexSnapshot(index))],
        cx,
    );
    let mut driver = crate::store::tests::LocalHostDriver::from_link(link.clone());
    driver.wait_until(cx, "child status", |cx| {
        store.read_with(cx, |store, _| store.agent_status("worker").is_some())
    });
    store.update(cx, |store, cx| store.show_alone("lead".into(), cx));

    let events = || Topic::SessionEvents {
        session_id: "lead".into(),
    };
    let spawn = |status, summary: Option<&str>| ThreadItem {
        id: "spawn".into(),
        parent_item_id: None,
        content: ItemContent::Subagent {
            agent_type: "explore".into(),
            description: "Survey".into(),
            status,
            summary: summary.map(str::to_owned),
            model: None,
            effort: None,
        },
    };
    let records = vec![
        AgentEvent::TurnStarted {
            turn_id: "turn".into(),
        }
        .into(),
        AgentEvent::ItemStarted(spawn(ItemStatus::InProgress, Some("Partway"))).into(),
    ];
    incoming
        .try_send(line(
            events(),
            ServerEvent::SessionSnapshot {
                total: 2,
                total_turns: 1,
                truncated: false,
                from: 0,
                end: 2,
                records,
            },
        ))
        .unwrap();
    let lines_of = |store: &WorkspaceStore, cx: &gpui::App| {
        let lines = Agents::of(store, "lead", cx)
            .with_results(store, "lead", cx)
            .lines();
        let line = |key: &str| {
            let line = lines.iter().find(|line| line.key == key).unwrap();
            (line.activity.clone(), line.preview.clone())
        };
        [line("worker"), line("spawn")]
    };
    driver.wait_until(cx, "the lead's transcript", |cx| {
        store.read_with(cx, |store, cx| {
            lines_of(store, cx)[1].0 == Some(LatestActivity::Thinking)
        })
    });
    assert_eq!(
        store.read_with(cx, |store, cx| lines_of(store, cx)),
        [
            (Some(command), None),
            (Some(LatestActivity::Thinking), None)
        ]
    );

    let (_, cx) = cx.add_window_view(|window, cx| {
        let panel = cx.new(|cx| super::AgentsPanel::new(store.clone(), Some("lead".into()), cx));
        gpui_base::Root::new(panel, window, cx)
    });
    cx.update(|_, cx| cx.set_reduce_motion(true));
    cx.simulate_resize(size(px(393.), px(852.)));
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert!(cx.debug_bounds("agent-activity-worker").is_some());
    assert!(cx.debug_bounds("agent-activity-spawn").is_some());

    let callback = format!(
        "{} reported.\nThe tests pass.",
        tcode_core::notice::callback_header("worker", "worker")
    );
    for message in [
        line(
            events(),
            ServerEvent::SessionEvent(
                AgentEvent::ItemCompleted(spawn(ItemStatus::Completed, Some("Surveyed"))).into(),
            ),
        ),
        line(
            events(),
            ServerEvent::SessionEvent(
                AgentEvent::ItemCompleted(ThreadItem {
                    id: "callback".into(),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: callback,
                        attachments: vec![],
                    },
                })
                .into(),
            ),
        ),
        line(
            Topic::Index,
            ServerEvent::IndexActivitiesChanged {
                upserts: [(
                    "worker".to_owned(),
                    activity(
                        Some(status(
                            AgentExecution::Finished,
                            AgentDelivery::AwaitingSettle,
                            AgentReport::Reported,
                        )),
                        None,
                    ),
                )]
                .into(),
                removed: vec![],
            },
        ),
    ] {
        incoming.try_send(message).unwrap();
    }
    driver.wait_until(cx, "the agents to settle", |cx| {
        store.read_with(cx, |store, cx| lines_of(store, cx)[0].1.is_some())
    });
    assert_eq!(
        store.read_with(cx, |store, cx| lines_of(store, cx)),
        [
            (None, Some("The tests pass.".to_owned())),
            (None, Some("Surveyed".to_owned()))
        ]
    );
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    assert!(cx.debug_bounds("agent-activity-worker").is_none());
    assert!(cx.debug_bounds("agent-activity-spawn").is_none());
    link.close();
}

#[gpui::test]
fn settle_all_button_sends_the_lead_command(cx: &mut TestAppContext) {
    use gpui::{px, size};
    use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};
    use tcode_protocol::{
        AgentStatus, ClientPayload, Command, SessionActivity, decode_client_line,
    };
    cx.update(crate::test_client::init);
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
    });
    let thread = |id: &str, parent: Option<&str>| {
        let mut meta = SessionMeta::new(ProviderKind::Codex, "/project".into(), None);
        meta.id = id.into();
        meta.title = id.into();
        meta.parent_session_id = parent.map(str::to_owned);
        meta
    };
    let mut index = IndexSnapshot {
        sessions: vec![thread("lead", None), thread("finished", Some("lead"))],
        projects: vec![],
        summary: Default::default(),
    };
    index.summary.activity.insert(
        "finished".into(),
        SessionActivity {
            agent: Some(AgentStatus {
                execution: AgentExecution::Finished,
                delivery: AgentDelivery::AwaitingSettle,
                report: AgentReport::Reported,
                run_started_at: None,
                run_completed_at: None,
            }),
            working: false,
            turn_running: false,
            background: Default::default(),
            unsettled: 0,
            wakes: Default::default(),
            waiting_for_approval: false,
            waiting_for_input: false,
            settle_blocked: false,
            failed: false,
            unread: false,
            last_activity_at: None,
            fork: tcode_protocol::ForkAvailability::Available,
            latest_activity: None,
        },
    );
    let index = tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
        request_id: None,
        topic: Topic::Index,
        event: ServerEvent::IndexSnapshot(index),
    }))
    .unwrap();
    crate::store::tests::seed_full_scope(&store, &incoming, vec![index], cx);
    let mut driver = crate::store::tests::LocalHostDriver::from_link(link.clone());
    driver.wait_until(cx, "child status", |cx| {
        store.read_with(cx, |store, _| store.agent_status("finished").is_some())
    });
    store.update(cx, |store, cx| store.show_alone("lead".into(), cx));
    let (_, cx) = cx.add_window_view(|window, cx| {
        let panel = cx.new(|cx| super::AgentsPanel::new(store.clone(), Some("lead".into()), cx));
        gpui_base::Root::new(panel, window, cx)
    });
    cx.simulate_resize(size(px(393.), px(852.)));
    // The list lays its rows out again at the width it is resized to on
    // the frame it asks for, which the app draws before a click lands.
    crate::test_frames::draw_settled(cx);
    let button = cx
        .debug_bounds("agents-settle-all")
        .expect("settle-all button is drawn");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    let mut commands = Vec::new();
    driver.wait_until(cx, "settle-all command", |_| {
        while let Ok(line) = outgoing.try_recv() {
            if let ClientPayload::Command(command) = decode_client_line(&line).unwrap().payload {
                commands.push(command);
            }
        }
        commands
            .iter()
            .any(|command| matches!(command, Command::SettleChildren { .. }))
    });
    assert_eq!(
        commands
            .into_iter()
            .filter(|command| matches!(command, Command::SettleChildren { .. }))
            .collect::<Vec<_>>(),
        vec![Command::SettleChildren {
            session_id: "lead".into()
        }]
    );
    link.close();
}
